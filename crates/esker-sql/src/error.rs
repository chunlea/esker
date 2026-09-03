//! The one place a SQLSTATE is chosen.
//!
//! Contract C3 (`docs/plans/phase-6a.md` §1) promises that the code a client sees is the code
//! PostgreSQL would have sent. That promise is only checkable if the mapping lives in one place, so
//! every error this crate reports is a [`SqlError`] and every `SqlError` knows its own code. Nothing
//! outside this module writes a five-character string into a wire message.
//!
//! [`SqlError::FeatureNotSupported`] carries contract C2: a statement Esker can parse but cannot
//! execute is this error, naming the feature, and never a syntax error and never a panic.

use std::fmt;

use crate::sqlstate;

/// The result type of everything in this crate that can fail on behalf of a client.
pub type Result<T> = std::result::Result<T, SqlError>;

/// How serious the condition is. PostgreSQL sends this twice in an `ErrorResponse` — once
/// localised (field `S`) and once not (field `V`) — and clients read the unlocalised one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The statement failed. Inside a transaction block, the transaction is now aborted.
    Error,
    /// The statement succeeded; something about it is worth saying.
    Warning,
    /// Purely informational.
    Notice,
    /// The connection is being closed and no further statement will be processed.
    Fatal,
}

impl Severity {
    /// The exact token PostgreSQL puts in the `V` field. Clients compare against these strings.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Notice => "NOTICE",
            Severity::Fatal => "FATAL",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can go wrong on behalf of a client, with the PostgreSQL condition it maps to.
///
/// The variants are conditions rather than call sites on purpose: two places that raise
/// "this table does not exist" must produce one code and one message shape, because a client
/// cannot see which line of ours it came from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SqlError {
    /// A `WITH` item referenced before it was written, or by itself.
    ///
    /// The whole three-part answer, verbatim: without the `DETAIL` a user reads `relation "a" does
    /// not exist` and goes looking for a missing table, where what is actually wrong is the
    /// *order* of two things they wrote. Measured — and the `HINT` names `WITH RECURSIVE`, which
    /// is the feature that would make it legal and which this node refuses by name.
    #[error("relation \"{0}\" does not exist")]
    ForwardCteReference(String),

    /// Two `WITH` items of one name.
    ///
    /// `42712`, the code two `FROM` entries of one name get — with a different sentence, because
    /// PostgreSQL says `WITH query name` where it says `table name`. Measured, both.
    #[error("WITH query name \"{0}\" specified more than once")]
    DuplicateCteName(String),

    /// A subquery has the wrong number of columns for where it was written.
    ///
    /// `42601` like a syntax error, and **not** [`SqlError::Syntax`], which prefixes its message
    /// with `syntax error:` — PostgreSQL's own sentence here has no such prefix. Two sentences
    /// share the variant because PostgreSQL uses two: a scalar subquery is `subquery must return
    /// only one column` and an `IN`/`ANY`/`ALL` is `subquery has too many columns`. Measured.
    #[error("{0}")]
    SubqueryColumns(&'static str),

    /// A subquery written where one value goes returned more than one row.
    ///
    /// PostgreSQL's own sentence, verbatim. It is raised **while the statement runs** rather than
    /// while it is planned, which is not a detail: the same statement is fine on a snapshot where
    /// the subquery matches one row and `21000` on the next, and a client that saw it at plan time
    /// would be told its SQL was wrong when its data had changed.
    #[error("more than one row returned by a subquery used as an expression")]
    CardinalityViolation,

    /// Contract C2. The statement parsed and we will not run it — the feature is named so the
    /// message reads the way PostgreSQL's own does.
    #[error("{0} is not supported")]
    FeatureNotSupported(String),

    /// The statement is not valid SQL. Contract C1 says this must never be the answer to
    /// something PostgreSQL 19 accepts; when it is, the statement belongs in the gap register.
    #[error("syntax error: {message}")]
    Syntax {
        /// What the parser objected to.
        message: String,
        /// One-based character offset, when the parser reported one. PostgreSQL sends this in the
        /// `P` field and `psql` uses it to draw the caret.
        position: Option<u32>,
        /// What to write instead, for the handful of spellings a user is likely to try because
        /// another database has them.
        ///
        /// PostgreSQL answers `42601` for those too, so the *code* is parity and nothing here is
        /// invented syntax. What a bare syntax error cannot carry is that this node has the
        /// feature under another name (`crate::parse`'s redirect table), and a user who wrote
        /// `CockroachDB`'s `AS OF SYSTEM TIME` has no other way to find out.
        hint: Option<&'static str>,
    },

    /// The statement nests deeper than the parser may safely descend
    /// (`crate::parse::MAX_NESTING_DEPTH`). PostgreSQL raises the same condition when
    /// `max_stack_depth` is exceeded.
    #[error("stack depth limit exceeded")]
    StatementTooComplex,

    /// No such table.
    #[error("relation \"{0}\" does not exist")]
    UndefinedTable(String),

    /// A `PRIMARY KEY` or `UNIQUE` clause naming a column the table does not have. PostgreSQL
    /// words this one differently from an ordinary missing column, and the extra three words are
    /// what tell a user to look at the constraint rather than at the column list.
    #[error("column \"{0}\" named in key does not exist")]
    UndefinedColumnInKey(String),

    /// No such table, said the way `DROP TABLE` says it. PostgreSQL words the same condition
    /// differently depending on the statement — a query says `relation`, a `DROP TABLE` says
    /// `table` — and both were captured rather than assumed.
    #[error("table \"{0}\" does not exist")]
    UndefinedTableForDrop(String),

    /// `ALTER TABLE t DISABLE TRIGGER x` naming a trigger that is not there.
    ///
    /// `42704 undefined_object`, and PostgreSQL names **both** the trigger and the table it looked
    /// on, which is the useful half — a trigger name is unique per table, not per schema, so the
    /// name alone would not say where it was looked for. This node has no triggers at all, so
    /// every name reaches this; `ALL` and `USER` are keywords in that position and do not.
    #[error("trigger \"{trigger}\" for table \"{table}\" does not exist")]
    UndefinedTrigger {
        /// The trigger named.
        trigger: String,
        /// The table it was looked for on.
        table: String,
    },

    /// A write to a `pg_catalog` relation, which is computed here and read-only everywhere.
    ///
    /// The sentence is a real server's, measured: `DROP TABLE pg_type`, `ALTER TABLE pg_type ADD
    /// COLUMN` and `CREATE INDEX … ON pg_type` all answer it. A real server does **not** refuse
    /// DML this way for a superuser — it lets one write `pg_type` and break the database — and
    /// this node refuses every write alike, because it has no roles and a computed relation has
    /// nothing to write to (`crate::catalog::pg_catalog`).
    #[error("permission denied: \"{0}\" is a system catalog")]
    SystemCatalog(&'static str),

    /// No such index.
    #[error("index \"{0}\" does not exist")]
    UndefinedIndex(String),

    /// The name exists and is the wrong kind of thing: `DROP TABLE` naming an index. Distinct from
    /// "does not exist", and a client told the wrong one would go looking for the wrong bug.
    #[error("\"{name}\" is not {expected}")]
    WrongObjectType {
        /// The name that resolved.
        name: String,
        /// What the statement needed, with its article: `a table`, `an index`.
        expected: &'static str,
        /// What it actually is, as the verb that removes it: `DROP TABLE`, `DROP INDEX`,
        /// `DROP SEQUENCE`.
        ///
        /// The `HINT` needs it, and `expected` alone cannot supply it: with three kinds of
        /// relation sharing one namespace, `DROP TABLE` over a name that is not a table has two
        /// different right answers. Measured — a real server hints `Use DROP SEQUENCE to remove a
        /// sequence.` for one and `Use DROP INDEX to remove an index.` for the other.
        found: &'static str,
    },

    /// `DROP INDEX` naming the index a primary key constraint owns. PostgreSQL refuses it and
    /// says what to drop instead; so do we, with the difference that here there is no index at all
    /// — the row key is the primary key — and the answer is the same either way.
    #[error("cannot drop index {index} because constraint {index} on table {table} requires it")]
    DependentObjectsStillExist {
        /// The constraint's name, which is also the index's.
        index: String,
        /// The table it is on.
        table: String,
    },

    /// No such column.
    #[error("column \"{0}\" does not exist")]
    UndefinedColumn(String),

    /// `CREATE TABLE` over a live name.
    #[error("relation \"{0}\" already exists")]
    DuplicateTable(String),

    /// Two columns of one table share a name.
    #[error("column \"{0}\" specified more than once")]
    DuplicateColumn(String),

    /// A bare column name that more than one table in the query has. PostgreSQL says "column
    /// reference", not "column", because what is ambiguous is the reference and not the column.
    #[error("column reference \"{0}\" is ambiguous")]
    AmbiguousColumn(String),

    /// `SELECT wrong.a FROM t` — a qualifier naming a table the query does not have. `42P01` like
    /// any other missing relation, and with PostgreSQL's own sentence for this shape of it.
    #[error("missing FROM-clause entry for table \"{0}\"")]
    MissingFromEntry(String),

    /// `SELECT t.a FROM t AS x` — a qualifier naming a table the query *does* have, under a name
    /// the alias took away.
    ///
    /// A different sentence from [`SqlError::MissingFromEntry`] and the same `42P01`, because the
    /// two are different mistakes: one is a table nobody put in the query, and this one is a table
    /// that is there under another name. PostgreSQL says which name, in a `HINT`, and both halves
    /// were measured (`tests/corpus/pg19_alias.txt`).
    #[error("invalid reference to FROM-clause entry for table \"{table}\"")]
    InvalidFromReference {
        /// The name the user wrote, which is the table's own.
        table: String,
        /// The alias that replaced it.
        alias: String,
    },

    /// `FROM t JOIN t` or `FROM a AS x JOIN b AS x` — two FROM entries a qualifier cannot tell
    /// apart.
    ///
    /// Refused rather than resolved to the first, which is what a scope keyed by name does without
    /// noticing: `t.id` would silently mean the outer one and a self-join would return the wrong
    /// column with nothing to say so.
    #[error("table name \"{0}\" specified more than once")]
    DuplicateTableName(String),

    /// `ALTER TABLE ... ADD COLUMN` naming a column the table already has. The same `42701` as
    /// above and a different sentence: PostgreSQL names the relation here, because the column it
    /// is talking about is one that already exists rather than one the statement repeated.
    #[error("column \"{column}\" of relation \"{relation}\" already exists")]
    DuplicateColumnInRelation {
        /// The column that is already there.
        column: String,
        /// The table it belongs to.
        relation: String,
    },

    /// The same, under `IF NOT EXISTS`: a notice, and PostgreSQL keeps `42701` on it rather than
    /// dropping to `00000` the way the `DROP ... IF EXISTS` notice does. Captured, because the
    /// asymmetry is not one anybody would invent.
    #[error("column \"{column}\" of relation \"{relation}\" already exists, skipping")]
    DuplicateColumnSkipping {
        /// The column that is already there.
        column: String,
        /// The table it belongs to.
        relation: String,
    },

    /// `ALTER TABLE` naming an index or a sequence. PostgreSQL names the *action* rather than
    /// saying "is not a table", because both are relations and the action is what cannot be
    /// performed on them.
    #[error("ALTER action {action} cannot be performed on relation \"{name}\"")]
    AlterActionOnWrongObject {
        /// The action, as PostgreSQL spells it: `ADD COLUMN`.
        action: &'static str,
        /// The relation that was named.
        name: String,
        /// What it is, plural, for the `DETAIL`: `indexes`, `sequences`. Measured — the two really
        /// are different sentences off one condition.
        kind: &'static str,
    },

    /// A duplicate reached a unique index.
    #[error("duplicate key value violates unique constraint \"{constraint}\"")]
    UniqueViolation {
        /// The constraint's name, which is what a client matches on.
        constraint: String,
        /// `Key (a, b)=(1, x)` for the `DETAIL` field, or `None` when the values are not to hand.
        ///
        /// PostgreSQL renders this with no quoting at all — a text value containing `, y)` comes
        /// out as `Key (a, b)=(1, x, y))`, unbalanced parentheses and all. Copied exactly, because
        /// a client that parses the field has been written against *that*.
        key: Option<String>,
    },

    /// A NULL reached a `NOT NULL` column.
    #[error("null value in column \"{0}\" violates not-null constraint")]
    NotNullViolation(String),

    /// A literal could not be read as its target type.
    #[error("invalid input syntax for type {ty}: \"{value}\"")]
    InvalidTextRepresentation {
        /// The PostgreSQL type name, as it appears in the message.
        ty: &'static str,
        /// The text that could not be read.
        value: String,
    },

    /// An integer literal is well-formed and too big. PostgreSQL phrases the two numeric ranges
    /// differently — this one leads with `value` and [`SqlError::FloatOutOfRange`] does not — and
    /// both are copied verbatim because a client may be matching on either.
    #[error("value \"{value}\" is out of range for type {ty}")]
    IntegerOutOfRange {
        /// The PostgreSQL type name, as it appears in the message.
        ty: &'static str,
        /// The text that would not fit.
        value: String,
    },

    /// An integer *literal* too large for the type it is going into. PostgreSQL words this one in
    /// three words, where the same overflow reached through the type's input function gets
    /// [`SqlError::IntegerOutOfRange`]'s longer message. Two paths, two messages, both captured —
    /// `INSERT INTO t (n) VALUES (2147483648)` is `integer out of range` and
    /// `WHERE n = '2147483648'` is `value "2147483648" is out of range for type integer`.
    #[error("{0} out of range")]
    IntegerLiteralOutOfRange(&'static str),

    /// More expressions in a `VALUES` tuple than there are columns to put them in. PostgreSQL
    /// calls this a *syntax* error, which it decides before looking at any of the values.
    #[error("INSERT has more expressions than target columns")]
    InsertTooManyExpressions,

    /// A float literal is well-formed and outside the type's range, in either direction:
    /// PostgreSQL raises this for `1e-400` as well as for `1e400`, rather than rounding to zero.
    #[error("\"{value}\" is out of range for type {ty}")]
    FloatOutOfRange {
        /// The PostgreSQL type name, as it appears in the message.
        ty: &'static str,
        /// The text that would not fit.
        value: String,
    },

    /// A datetime literal could not be read. The datetime types have their own condition
    /// (`22007`), not the `22P02` every other type uses.
    #[error("invalid input syntax for type {ty}: \"{value}\"")]
    InvalidDatetimeFormat {
        /// The PostgreSQL type name, as it appears in the message.
        ty: &'static str,
        /// The text that could not be read.
        value: String,
    },

    /// Two types with no cast between them: `42846`.
    ///
    /// Decided **before** a value is read, which is what makes it different from an input error.
    /// `'2020-01-01'::date::int` is this on a real server and not a `22P02` about the digits —
    /// the Julian day a `date` holds is an implementation detail with no cast to reach it, in
    /// either direction.
    #[error("cannot cast type {from} to {to}")]
    CannotCast {
        /// The source type, as PostgreSQL names it in the message.
        from: &'static str,
        /// The target type.
        to: &'static str,
    },

    /// A datetime field is outside its own range — a thirteenth month, a thirtieth of February.
    #[error("date/time field value out of range: \"{value}\"")]
    DatetimeFieldOutOfRange {
        /// The text that could not be read.
        value: String,
        /// Whether to add PostgreSQL's `DateStyle` hint.
        ///
        /// **It appears only when the offending field could have been a day.** `2020-13-01` gets
        /// it, because a 13 is a plausible day under `DMY` and the user may have meant one;
        /// `2020-02-30` does not, because a 30 is not a plausible month under anything. Measured
        /// for `date` and for `timestamp`, which follow the same rule.
        datestyle_hint: bool,
    },

    /// Every field was in range and the day or instant they name is not: past 294276 AD for a
    /// `timestamp`, past 5874897 AD for a `date`, or before 4714 BC for either.
    #[error("{ty} out of range: \"{value}\"")]
    DatetimeOutOfRange {
        /// The type name **as this message spells it**, which is not always the type's own name:
        /// `timestamp out of range` for both zone variants, where their syntax errors say
        /// `timestamp without time zone` and `timestamp with time zone`. Measured.
        ty: &'static str,
        /// The text that could not be read.
        value: String,
    },

    /// A time zone displacement past `±15:59`. Its own condition, not a field overflow.
    #[error("time zone displacement out of range: \"{0}\"")]
    TimeZoneDisplacementOutOfRange(String),

    /// A `bytea` hexadecimal literal contains something that is not a hexadecimal digit.
    #[error("invalid hexadecimal digit: \"{0}\"")]
    InvalidHexDigit(char),

    /// A `bytea` hexadecimal literal has a half byte at the end.
    #[error("invalid hexadecimal data: odd number of digits")]
    OddHexDigits,

    /// A `bytea` escape-format literal has a backslash that starts nothing valid. PostgreSQL does
    /// not quote the input back in this one, which a capture is the only way to know.
    #[error("invalid input syntax for type bytea")]
    InvalidByteaFormat,

    /// Bytes arrived that are not valid UTF-8, which is the server encoding.
    #[error("invalid byte sequence for encoding \"UTF8\": 0x{0:02x}")]
    InvalidByteSequence(u8),

    /// A value is being put in a column of another type, and no assignment cast covers it.
    #[error(
        "column \"{column}\" is of type {column_type} but expression is of type {expression_type}"
    )]
    DatatypeMismatchInColumn {
        /// The column being assigned to.
        column: String,
        /// Its type, as PostgreSQL names it.
        column_type: &'static str,
        /// The expression's type, as PostgreSQL names it — `integer` for a small constant, not
        /// `bigint`.
        expression_type: &'static str,
    },

    /// A `$1` with nothing bound to it. The simple query protocol has no way to carry one, so a
    /// parameter in a `Query` message is always this.
    #[error("there is no parameter ${0}")]
    UndefinedParameter(u32),

    /// An index expression whose value is not a function of the row alone.
    ///
    /// PostgreSQL words it about the *function* rather than about the expression, and this copies
    /// the sentence exactly because a client matching on it would not recognise anything else. It
    /// is the answer there for a volatile function (`nextval`) **and** for a merely stable one
    /// (`format_type`, `pg_get_indexdef`) — measured — which is every function this crate has
    /// apart from `lower` and `upper`.
    #[error("functions in index expression must be marked IMMUTABLE")]
    NotImmutableInIndex,

    /// A **qualified** column reference — `o.nosuch` — that the named table does not have.
    ///
    /// Three sentences for one condition, and all three are PostgreSQL's, captured rather than
    /// guessed: a bare reference is `column "nosuch" does not exist`, this one is `column
    /// o.nosuch does not exist` — dotted and *unquoted* — and an `UPDATE`'s `SET` target is
    /// `column "nosuch" of relation "o" does not exist`. A client that matches on the text sees a
    /// different one in each place, so writing one of them everywhere would be wrong in two.
    #[error("column {qualifier}.{column} does not exist")]
    UndefinedQualifiedColumn {
        /// The table the reference named.
        qualifier: String,
        /// The column it asked that table for.
        column: String,
    },

    /// A column named in a statement about one relation. PostgreSQL says which relation here,
    /// where a bare column reference elsewhere gets the shorter message.
    #[error("column \"{column}\" of relation \"{relation}\" does not exist")]
    UndefinedColumnInRelation {
        /// The column that is not there.
        column: String,
        /// The relation it was looked for in.
        relation: String,
    },

    /// A NULL reached a `NOT NULL` column, with the relation named the way PostgreSQL names it.
    #[error(
        "null value in column \"{column}\" of relation \"{relation}\" violates not-null constraint"
    )]
    NotNullViolationInRelation {
        /// The column.
        column: String,
        /// The table it belongs to.
        relation: String,
        /// The whole offending row for the `DETAIL` field, rendered the way PostgreSQL renders it:
        /// values joined with `, ` and `null` for a NULL, with no quoting.
        row: Option<String>,
    },

    /// A negative `LIMIT` or `OFFSET`. They carry *different* codes — `2201W` and `2201X` — so a
    /// client is told which clause it got wrong.
    #[error("{0} must not be negative")]
    NegativeLimit(&'static str),

    /// An operator applied to types it is not defined for, named the way PostgreSQL names it.
    #[error("operator does not exist: {left} {op} {right}")]
    UndefinedOperator {
        /// The left operand's type.
        left: &'static str,
        /// The operator symbol.
        op: &'static str,
        /// The right operand's type.
        right: &'static str,
    },

    /// An aggregate applied to a type it has no form for: `sum(text)`, `min(boolean)`.
    ///
    /// **Not a missing feature.** PostgreSQL 19 has no `sum(text)` and no `min(boolean)` either —
    /// measured, `tests/corpus/pg19_aggregate.txt` — so this is contract C3 parity and refusing it
    /// is being right rather than being incomplete. Implementing an ordering of `f` and `t` would
    /// have been the divergence.
    #[error("function {func}({argument}) does not exist")]
    UndefinedAggregate {
        /// The aggregate's name, lower case, as PostgreSQL writes it in the message.
        func: &'static str,
        /// The argument's type, in the name PostgreSQL talks about it by — `timestamp with time
        /// zone`, not `timestamptz`.
        argument: &'static str,
    },

    /// A call shape PostgreSQL's grammar rejects and `sqlparser` reads: `count(*, 1)`.
    ///
    /// A syntax error the *lowering* raises rather than the parser, which is unusual enough to
    /// justify a variant of its own. It is not [`SqlError::Syntax`] because that one renders
    /// `sqlparser`'s message behind a `syntax error:` prefix, and here the exact sentence a real
    /// server sends is **known** — it was measured, both spellings: `count(*, 1)` names the comma
    /// and `count(1, *)` names the star.
    ///
    /// The second place it is raised is an **index key**, where the token is read off the
    /// expression rather than being one of a fixed pair — which is why this carries a `String`.
    /// PostgreSQL's `index_elem` is `ColId | func_expr_windowless | '(' a_expr ')'`, so
    /// `ON t (a + 1)` is a syntax error there while `sqlparser` parses it happily, and accepting
    /// it would build an index a real server refuses to create
    /// (`crate::parse::lower::index_elem_token`).
    #[error("syntax error at or near \"{0}\"")]
    SyntaxAtOrNear(String),

    /// `ORDER BY <name>` where more than one **output** column is called that.
    ///
    /// A different sentence from [`SqlError::AmbiguousColumn`] and about a different thing: the
    /// columns in question are the ones the target list produced, not the ones the tables have.
    /// `SELECT l.id, r.id, id FROM l JOIN r USING (id) ORDER BY id` is this — the `id` in the
    /// target list is unambiguous, and the three columns it comes back with are not. Measured.
    #[error("ORDER BY \"{0}\" is ambiguous")]
    AmbiguousOrderBy(String),

    /// `USING (c)` where one of the two tables has no column `c`.
    ///
    /// PostgreSQL names **which side** is missing it, and that is the useful half: a typo and a
    /// join between the wrong two tables look identical without it.
    #[error("column \"{column}\" specified in USING clause does not exist in {side} table")]
    UsingColumnMissing {
        /// The column named in the clause.
        column: String,
        /// `left` or `right`.
        side: &'static str,
    },

    /// `SAVEPOINT`, `ROLLBACK TO SAVEPOINT` or `RELEASE SAVEPOINT` with no block open.
    ///
    /// The verb is **PostgreSQL's own**, not the user's: `RELEASE s` outside a block says
    /// `RELEASE SAVEPOINT can only be used in transaction blocks`, naming the full form whichever
    /// spelling arrived. Measured, all three.
    #[error("{0} can only be used in transaction blocks")]
    OutsideTransactionBlock(&'static str),

    /// `ROLLBACK TO` or `RELEASE` naming a savepoint that is not on the stack — because it never
    /// was, or because it has been released.
    #[error("savepoint \"{0}\" does not exist")]
    NoSuchSavepoint(String),

    /// `currval` or `lastval` before this session has taken a value.
    ///
    /// Session state, and PostgreSQL says so in the message: the sequence may well have a value,
    /// just not one *this* connection asked for. `None` is `lastval()`, which names no sequence
    /// because it is about the session and not about one of them.
    #[error("{}", match .0 {
        Some(name) => format!("currval of sequence \"{name}\" is not yet defined in this session"),
        None => "lastval is not yet defined in this session".to_owned(),
    })]
    SequenceNotYetDefined(Option<String>),

    /// `setval` below a sequence's minimum. PostgreSQL prints the whole permitted range.
    #[error(
        "setval: value {value} is out of bounds for sequence \"{sequence}\" \
         (1..9223372036854775807)"
    )]
    SetvalOutOfBounds {
        /// The sequence, which PostgreSQL names.
        sequence: String,
        /// What was asked for.
        value: i64,
    },

    /// A value written into a `GENERATED ALWAYS AS IDENTITY` column.
    ///
    /// PostgreSQL's own sentence, its `DETAIL` and its `HINT`, all three measured: the hint names
    /// `OVERRIDING SYSTEM VALUE`, which is the clause that takes the value anyway.
    #[error("cannot insert a non-DEFAULT value into column \"{column}\"")]
    GeneratedAlways {
        /// The column the value was written into.
        column: String,
    },

    /// `count()` — the one aggregate that takes no argument, called as though it took one.
    /// PostgreSQL answers `42809` here rather than `42883`, and says which spelling works.
    #[error("count(*) must be used to call a parameterless aggregate function")]
    ParameterlessAggregate,

    /// An aggregate called with a number of arguments it has no form for. A **different** `DETAIL`
    /// from [`SqlError::UndefinedAggregate`]: PostgreSQL distinguishes the wrong *number* of
    /// arguments from the wrong *types*, and both sentences were captured.
    #[error("function {func}({arguments}) does not exist")]
    UndefinedAggregateArity {
        /// The aggregate's name.
        func: &'static str,
        /// The argument types written, comma-separated, as PostgreSQL prints them: measured,
        /// `count(n, g)` comes back as `function count(bigint, text) does not exist`.
        arguments: String,
    },

    /// `sum(int8)` overflowing.
    ///
    /// The same `22003` and the same three words as any other `int8` overflow — measured,
    /// `9223372036854775807::int8 + 1` says exactly this — because a client cannot tell which
    /// addition it was. What is worth knowing is that **PostgreSQL never raises it here**: its
    /// `sum(bigint)` is `numeric` and cannot overflow, so this is the visible edge of ADR 0031's
    /// declared divergence rather than a shared failure.
    #[error("bigint out of range")]
    BigintOutOfRange,

    /// A column that is neither a grouping key nor inside an aggregate, in a query that groups.
    ///
    /// The name is **qualified** — `agg.n`, not `n` — because that is what a real server prints
    /// and because in a join it is the only form that says which table.
    #[error(
        "column \"{0}\" must appear in the GROUP BY clause or be used in an aggregate function"
    )]
    GroupingError(String),

    /// An aggregate written where a group does not exist yet: in `WHERE`, or inside another
    /// aggregate. PostgreSQL words the two differently and both are captured.
    #[error("{0}")]
    AggregateNotAllowed(&'static str),

    /// `ORDER BY`, `GROUP BY` or `SELECT DISTINCT` naming something the target list does not have.
    #[error("{0}")]
    InvalidColumnReference(String),

    /// An operator or function met types it is not defined for.
    #[error("{0}")]
    DatatypeMismatch(String),

    /// Two transactions wrote the same key and this one lost the race. The client is expected to
    /// retry; the executor turns this into a `23505` when the key it lost was a unique index entry,
    /// because from the user's point of view that is a duplicate and not a race.
    #[error("could not serialize access due to concurrent update: {message}")]
    SerializationFailure {
        /// What the store said.
        message: String,
        /// **Which key lost**, when the store answered per key — which `Prewrite` does
        /// (`docs/txn-spec.md` §6.1). `None` means the refusing method does not answer per key,
        /// not that no key lost.
        ///
        /// The executor needs it to tell two events apart that are one event to the layer below:
        /// a lost race on an ordinary row is this error, and a lost race on a *unique index
        /// entry* is the `23505` the user actually caused.
        key: Option<Vec<u8>>,
    },

    /// A request went out and no usable answer came back. Whether it was applied is genuinely
    /// unknown, and saying so is the only honest answer — reporting success would be a lie and
    /// reporting failure would be a different one.
    #[error("the transaction's outcome is unknown: {0}")]
    OutcomeUnknown(String),

    /// The store could not be reached, or would not answer in time.
    #[error("could not reach the store: {0}")]
    StoreUnavailable(String),

    /// `CREATE ... IF NOT EXISTS` for something that is already there. A notice: the statement
    /// succeeded and did nothing.
    #[error("relation \"{0}\" already exists, skipping")]
    AlreadyExistsSkipping(String),

    /// `DROP ... IF EXISTS` for something that is not there. Also a notice — and one that carries
    /// SQLSTATE `00000`, where the notice above carries `42P07`. The asymmetry is PostgreSQL's and
    /// was captured, not assumed.
    #[error("{kind} \"{name}\" does not exist, skipping")]
    DoesNotExistSkipping {
        /// The object word PostgreSQL uses here — `table`, `index`. Note that the *already
        /// exists* notice says `relation` for both.
        kind: &'static str,
        /// The name that was not found.
        name: String,
    },

    /// An identifier longer than 63 bytes. PostgreSQL truncates and carries on, so this is a
    /// notice and the statement still runs against the shortened name.
    #[error("identifier \"{original}\" will be truncated to \"{truncated}\"")]
    IdentifierTruncated {
        /// As the client wrote it.
        original: String,
        /// As it will be stored.
        truncated: String,
    },

    /// A statement arrived after an error inside a transaction block.
    #[error("current transaction is aborted, commands ignored until end of transaction block")]
    InFailedTransaction,

    /// `BEGIN` inside a transaction block. PostgreSQL sends a warning and stays in the
    /// transaction rather than failing, which a captured session confirms.
    #[error("there is already a transaction in progress")]
    ActiveTransaction,

    /// `COMMIT` or `ROLLBACK` outside a transaction block. PostgreSQL sends this as a warning and
    /// carries on, which is what the session layer does with it.
    #[error("there is no transaction in progress")]
    NoActiveTransaction,

    /// `SET TRANSACTION` outside a transaction block. A **warning**, and PostgreSQL sends it and
    /// then fails the statement for a second reason — both, in that order, which is why this is
    /// its own condition and not a variant of [`SqlError::NoActiveTransaction`].
    #[error("SET TRANSACTION can only be used in transaction blocks")]
    SetTransactionOutsideBlock,

    /// `SET TRANSACTION SNAPSHOT` where a snapshot cannot be imported. PostgreSQL's own sentence,
    /// and the one it sends outside a transaction block after the warning above.
    ///
    /// Inside a block this node never raises it: Percolator gives snapshot isolation, which is
    /// PostgreSQL's `REPEATABLE READ`, so the precondition holds by construction
    /// (`docs/adr/0021-time-machine.md`).
    #[error(
        "a snapshot-importing transaction must have isolation level SERIALIZABLE or REPEATABLE READ"
    )]
    SnapshotIsolationRequired,

    /// `CREATE`/`DROP INDEX CONCURRENTLY` inside a transaction block.
    ///
    /// PostgreSQL's own refusal, captured: `25001 DROP INDEX CONCURRENTLY cannot run inside a
    /// transaction block`. The reason is the same on both servers and worth stating — a concurrent
    /// build is *many* transactions, so it cannot be part of one, and a block that could roll it
    /// back would be a block that could roll back half a schema change.
    #[error("{0} cannot run inside a transaction block")]
    ConcurrentlyInTransactionBlock(&'static str),

    /// `SET TRANSACTION SNAPSHOT` after the block has already read something. Exactly right, and
    /// the reason it is worth copying: a `start_ts` cannot change under a transaction that has
    /// already read at it.
    #[error("SET TRANSACTION SNAPSHOT must be called before any query")]
    SnapshotAfterQuery,

    /// A function called with an argument of the wrong **type**: `42883`.
    ///
    /// A different `DETAIL` and a `HINT` from [`SqlError::UndefinedFunction`], which is the
    /// wrong *arity* — PostgreSQL distinguishes the two and says "argument types" for one and
    /// "number of arguments" for the other. Measured on `lower(1)` against `lower('a','b')`.
    #[error("function {0} does not exist")]
    UndefinedFunctionTypes(String),

    /// A **schema-qualified** function name that names nothing: `42883`, and with **no `DETAIL`**.
    ///
    /// The third of the three shapes, and the one that says least. PostgreSQL's other two describe
    /// the candidates it nearly matched — "No function of that name accepts the given argument
    /// types." for a wrong type, "…the given number of arguments." for a wrong arity — and here
    /// there are no candidates to describe, because the schema itself holds nothing by that name.
    /// Measured: `public.obj_description('x'::regclass)` is one line and no more.
    #[error("function {0} does not exist")]
    UndefinedQualifiedFunction(String),

    /// A function this node has under a name but not with that signature: `42883`.
    ///
    /// PostgreSQL resolves a function by name **and** argument types, so the wrong arity is not a
    /// badly-called function — it is a function that does not exist, and the message spells the
    /// types out: `function current_schema(boolean) does not exist`. The `0A000` a function this
    /// node has never heard of gets is a different answer for a different condition.
    #[error("function {0} does not exist")]
    UndefinedFunction(String),

    /// A `jsonb` document containing a NUL escape: `22P05`.
    ///
    /// The one input that tells `json` from `jsonb`. `json` stores it, because `json` stores the
    /// text; `jsonb`'s stored form is text and a NUL cannot be in one, so it refuses — and casting
    /// a stored `json` that contains one to `jsonb` raises this later, which is what makes `json`'s
    /// permissiveness safe rather than a trap. Measured, and the only `22P05` in this project.
    #[error("unsupported Unicode escape sequence")]
    UnsupportedUnicodeEscape,

    /// A type name that names no type on this node: `42704`.
    ///
    /// What `'nope'::regtype` answers on a real server, word for word. It is also what this node
    /// answers for a type PostgreSQL *has* and it does not — `numeric`, an array — which is a
    /// declared divergence rather than an oversight: answering `1700` would hand a client the OID
    /// of a type this node can neither store nor send, and `crates/esker-sql/src/catalog/
    /// pg_catalog.rs` already refuses to list one for exactly that reason.
    #[error("type \"{0}\" does not exist")]
    UndefinedType(String),

    /// `42601` from PostgreSQL's **type-name** parser, which is a different grammar from a
    /// statement's: `'timestamp(-1)'::regtype` stops at the sign and `'integer(4)'::regtype` stops
    /// at the parenthesis, because a typmod argument is an unsigned integer and `integer` takes no
    /// typmod at all. Its own variant because [`SqlError::Syntax`] prefixes `syntax error: ` and
    /// this message *is* `syntax error at or near "…"`.
    #[error("syntax error at or near \"{0}\"")]
    TypeNameSyntax(String),

    /// A typmod on a type that takes none, where the name is **not** one of PostgreSQL's type
    /// keywords: `42601 type modifier is not allowed for type "jsonb"`.
    ///
    /// The other half of [`SqlError::TypeNameSyntax`], and which one you get is decided by the
    /// grammar rather than by the type: `json(10)` is a syntax error and `jsonb(10)` is this,
    /// because `json` is a keyword in the type production and `jsonb` is an ordinary identifier.
    /// Measured for every spelling this node has.
    #[error("type modifier is not allowed for type \"{0}\"")]
    TypeModifierNotAllowed(String),

    /// A type name with nothing in it: `42601 invalid type name ""`. Not `42704` — PostgreSQL
    /// refuses to look it up rather than failing to find it.
    #[error("invalid type name \"{0}\"")]
    InvalidTypeName(String),

    /// A value longer than its column's declared length: `22001`.
    ///
    /// The type is spelled as `format_type` writes it — `character varying(5)`, `character(3)` —
    /// and **not** the way the two messages below spell it. PostgreSQL really does use two
    /// vocabularies for one type, `tests/corpus/pg19_typmod.txt` has both, and neither was
    /// guessed.
    #[error("value too long for type {0}")]
    StringDataRightTruncation(String),

    /// A function **name** this server does not have at any arity: `42883`.
    ///
    /// The fourth of PostgreSQL's four shapes for this code, and the one that says the name itself
    /// is unknown — `DETAIL: There is no function of that name.` The other three are a known name
    /// with the wrong *arity* ([`SqlError::UndefinedFunction`], "…the given number of arguments"),
    /// with the wrong *types* ([`SqlError::UndefinedFunctionTypes`], "…the given argument types"
    /// plus a cast HINT), and a **schema-qualified** name that matches nothing
    /// ([`SqlError::UndefinedQualifiedFunction`], no DETAIL at all). All four measured.
    ///
    /// `uuid_generate_v4()` before its extension is installed is this one: not a signature
    /// mismatch, but a name that is not there yet.
    #[error("function {0} does not exist")]
    UndefinedFunctionName(String),

    /// `CREATE EXTENSION x` where `x` is already installed: `42710 duplicate_object`.
    ///
    /// `IF NOT EXISTS` turns this into a plain success — that is the **only** thing the clause
    /// covers, and it does nothing for an extension the build does not have
    /// ([`SqlError::ExtensionNotAvailable`]).
    #[error("extension \"{0}\" already exists")]
    DuplicateExtension(String),

    /// `CREATE EXTENSION x` where this build has no `x`: `0A000`, with PostgreSQL's own HINT.
    ///
    /// Not `42704` and not a syntax error: a real server calls an extension it cannot find on
    /// disk a *feature it does not have*, which is what it is here too. `IF NOT EXISTS` does
    /// **not** cover it — measured, the message and the HINT are identical with and without the
    /// clause — because the clause is about existence and this is about availability.
    #[error("extension \"{0}\" is not available")]
    ExtensionNotAvailable(String),

    /// A constraint name the relation already has: `42710`.
    #[error("constraint \"{constraint}\" for relation \"{relation}\" already exists")]
    DuplicateConstraint {
        /// The name that collided.
        constraint: String,
        /// The table it is on.
        relation: String,
    },

    /// A row a `CHECK` refuses: `23514`.
    #[error("new row for relation \"{relation}\" violates check constraint \"{constraint}\"")]
    CheckViolation {
        /// The constraint's name, given or derived.
        constraint: String,
        /// The table.
        relation: String,
        /// The row, for the `DETAIL` line PostgreSQL sends with it.
        row: String,
    },

    /// A child row pointing at a parent row that is not there: `23503`, from the child's side.
    #[error(
        "insert or update on table \"{relation}\" violates foreign key constraint \"{constraint}\""
    )]
    ForeignKeyViolation {
        /// The **child** table, whose row was written.
        relation: String,
        /// The constraint's name, given or derived.
        constraint: String,
        /// `Key (p)=(99) is not present in table "fxp".`
        detail: String,
    },

    /// A parent row something still points at: `23503`, from the parent's side.
    ///
    /// One SQLSTATE with the one above and a **different sentence**, which names both tables. An
    /// implementation with one message for both looks right on half the cases; measured on
    /// PostgreSQL 19, `tests/corpus/pg19_foreign_key.txt`.
    #[error(
        "update or delete on table \"{relation}\" violates foreign key constraint \
         \"{constraint}\" on table \"{child}\""
    )]
    ForeignKeyStillReferenced {
        /// The **parent** table, whose row was deleted or re-keyed.
        relation: String,
        /// The constraint's name.
        constraint: String,
        /// The child table that still holds a reference.
        child: String,
        /// `Key (id)=(2) is still referenced from table "fxc".`
        detail: String,
    },

    /// A `FOREIGN KEY` whose referenced columns have no unique index behind them: `42830`.
    #[error("there is no unique constraint matching given keys for referenced table \"{0}\"")]
    NoUniqueConstraintForReference(String),

    /// A column named in a `FOREIGN KEY` that the table does not have: `42703`, with a sentence of
    /// its own rather than the plain "column … does not exist".
    #[error("column \"{0}\" referenced in foreign key constraint does not exist")]
    UndefinedColumnInForeignKey(String),

    /// A table a `FOREIGN KEY` still references: `2BP01`, the same code as the index one above and
    /// a different sentence — PostgreSQL words this class per dependency.
    #[error("cannot drop table {relation} because other objects depend on it")]
    DependentTable {
        /// The table that cannot be dropped.
        relation: String,
        /// `constraint fxc_p on table fxc depends on table fxp`
        detail: String,
    },

    /// A `numeric` special cast to an integer: **`0A000`**, not `22003`.
    ///
    /// The one SQLSTATE nobody would predict here — `'NaN'::numeric::int` is
    /// `cannot convert NaN to integer` with a *feature-not-supported* code, where the same cast
    /// of a value that is merely too large is `22003 integer out of range`. Measured; PostgreSQL
    /// treats "this value has no integer at all" as a missing conversion rather than an overflow.
    #[error("cannot convert {value} to {target}")]
    CannotConvert {
        /// `NaN` or `infinity` — PostgreSQL spells the first with capitals and the second without.
        value: &'static str,
        /// The target type, as it is named in the message.
        target: &'static str,
    },

    /// A `numeric` value that does not fit its declared precision and scale: `22003`.
    ///
    /// Two conditions with one code and **two different sentences**, which is what makes the
    /// detail worth carrying rather than deriving: an infinity cannot be held by any typmod at
    /// all, where a finite value that is merely too large names the bound it broke. `NaN` fits
    /// every typmod and reaches neither.
    #[error("numeric field overflow")]
    NumericFieldOverflow {
        /// `A field with precision 10, scale 2 must round to an absolute value less than 10^8.`
        detail: String,
    },

    /// A declared `numeric` precision outside `1..=1000`: `22023`. PostgreSQL shouts the type
    /// name in this one.
    #[error("NUMERIC precision {0} must be between 1 and 1000")]
    NumericPrecisionOutOfRange(i32),

    /// A declared `numeric` scale outside `-1000..=1000`: `22023`, and the scale is **signed** —
    /// `numeric(10,-2)` is a real type.
    #[error("NUMERIC scale {0} must be between -1000 and 1000")]
    NumericScaleOutOfRange(i32),

    /// A `float(p)` whose precision is outside `1..=53`: `22023`.
    ///
    /// Its own message, not [`SqlError::TypeLengthTooSmall`]'s: PostgreSQL says "precision" and
    /// "bit"/"bits" here where it says "length" for a string, and both were measured — `float(0)`
    /// is `must be at least 1 bit` and `float(54)` is `must be less than 54 bits`.
    #[error("precision for type float must be at least 1 bit")]
    FloatPrecisionTooSmall,

    /// The other end of the same range.
    #[error("precision for type float must be less than 54 bits")]
    FloatPrecisionTooLarge,

    /// A declared length below PostgreSQL's floor of one: `22023`.
    ///
    /// Spelled `varchar` and `char`, which is the short form — the opposite of
    /// [`SqlError::StringDataRightTruncation`] right above it. Measured on 19beta1:
    /// `varchar(0)` is `length for type varchar must be at least 1`.
    #[error("length for type {0} must be at least 1")]
    TypeLengthTooSmall(&'static str),

    /// A declared length above PostgreSQL's ceiling: `22023`.
    #[error("length for type {0} cannot exceed {1}")]
    TypeLengthTooLarge(&'static str, u32),

    /// A string that cannot be a snapshot identifier at all.
    ///
    /// Distinct from [`SqlError::SnapshotDoesNotExist`], and the distinction is PostgreSQL's:
    /// `'nope'` is `22023` and a well-formed `'00000003-0000001B-1'` that is not there is `42704`.
    /// Captured, because nobody would invent two codes for one apparent condition.
    #[error("invalid snapshot identifier: \"{0}\"")]
    InvalidSnapshotIdentifier(String),

    /// A snapshot identifier that is well formed and names nothing.
    #[error("snapshot \"{0}\" does not exist")]
    SnapshotDoesNotExist(String),

    /// A `SET` whose value the parameter cannot read.
    #[error("invalid value for parameter \"{name}\": \"{value}\"")]
    InvalidParameterValue {
        /// The parameter, as the user spelled it.
        name: &'static str,
        /// The text it would not take.
        value: String,
    },

    /// A `SET` whose value a boolean parameter cannot read. PostgreSQL gives this one a sentence
    /// rather than a list of values, which is the difference a client sees between a boolean and
    /// an enum. Measured, `tests/corpus/pg19_set.txt`.
    #[error("parameter \"{0}\" requires a Boolean value")]
    NonBooleanParameter(&'static str),

    /// A `SET` of a parameter that exists and is fixed. `55P02`, and the reason it is not `42704`:
    /// a parameter that cannot be changed is a different answer from one that is not there.
    #[error("parameter \"{0}\" cannot be changed")]
    CannotChangeParameter(&'static str),

    /// `SET standard_conforming_strings = off`, which is **PostgreSQL's own refusal** — its
    /// message and its `0A000`, not a gap of ours. This crate's string lexer is
    /// standard-conforming and no setting makes it otherwise.
    #[error("non-standard string literals are not supported")]
    NonStandardStringLiterals,

    /// A `SET` whose value is well formed and outside what the parameter admits.
    ///
    /// PostgreSQL's own sentence for this, measured rather than recalled: `-5 ms is outside the
    /// valid range for parameter "lock_timeout" (0 ms .. 2147483647 ms)`. It is the shape a
    /// travel-window refusal wants, because what the user needs back is the pair of instants they
    /// *can* ask for (`docs/adr/0021-time-machine.md` Decision 1).
    #[error("{value} is outside the valid range for parameter \"{name}\" ({low} .. {high})")]
    ParameterOutOfRange {
        /// What was asked for.
        value: String,
        /// The parameter.
        name: &'static str,
        /// The low end, inclusive.
        low: String,
        /// The high end, inclusive.
        high: String,
    },

    /// A parameter this node does not have. PostgreSQL's answer for an un-namespaced custom `SET`
    /// and for a `SHOW` of anything it was never told about.
    #[error("unrecognized configuration parameter \"{0}\"")]
    UnrecognizedParameter(String),

    /// A write in a transaction that may not write, which here is every transaction reading the
    /// past. PostgreSQL names the command, so a user sees which of several statements it was.
    #[error("cannot execute {0} in a read-only transaction")]
    ReadOnlyTransaction(&'static str),

    /// This node's schema lease has run out and it could not renew it, so it will not write
    /// ([ADR 0028](../../docs/adr/0028-the-schema-lease.md)).
    ///
    /// **Fail closed**, and the message says which half is refused, because the other half still
    /// works: reads are never gated by the lease. `40003 statement_completion_unknown` would be
    /// wrong — nothing was attempted — and `08006` would blame the store, which may be perfectly
    /// reachable. PostgreSQL has no condition for "this node is not allowed to write right now",
    /// so this is `25006`, which is exactly what it means to the client: a transaction that may
    /// not write.
    #[error(
        "cannot execute {command} in a read-only transaction: this node's schema lease has \
         expired and the placement driver is unreachable"
    )]
    SchemaLeaseExpired {
        /// The command, named as PostgreSQL names it.
        command: &'static str,
    },

    /// A query needs more of a bounded resource than this node will give it.
    #[error("{0}")]
    ConfigurationLimitExceeded(String),

    /// The frontend sent something the protocol does not allow.
    #[error("{0}")]
    ProtocolViolation(String),

    /// A prepared statement name that was never parsed.
    #[error("prepared statement \"{0}\" does not exist")]
    InvalidSqlStatementName(String),

    /// A portal name that does not exist.
    #[error("portal \"{0}\" does not exist")]
    InvalidCursorName(String),

    /// Authentication failed. Fatal: the connection ends, and PostgreSQL says so with this exact
    /// message so a client can tell a wrong password from a missing role.
    #[error("password authentication failed for user \"{0}\"")]
    InvalidPassword(String),

    /// Bytes came back from storage that could not be read as the row or key they should be.
    /// Distinct from [`SqlError::Internal`] because it says where to look: the data, not the code.
    #[error("corrupt data: {0}")]
    DataCorrupted(String),

    /// A bug here, not a mistake there. Nothing driven by user input may produce this.
    #[error("internal error: {0}")]
    Internal(String),
}

impl SqlError {
    /// The five-character SQLSTATE a client will branch on.
    #[must_use]
    // One arm per condition, and that is the point: a table of variants against the five-character
    // codes clients branch on reads as a table and would read as nothing if it were split in three.
    #[allow(clippy::too_many_lines)]
    pub fn sqlstate(&self) -> &'static str {
        match self {
            SqlError::FeatureNotSupported(_)
            | SqlError::CannotConvert { .. }
            | SqlError::NonStandardStringLiterals
            | SqlError::ExtensionNotAvailable(_)
            | SqlError::SnapshotIsolationRequired => sqlstate::FEATURE_NOT_SUPPORTED,
            SqlError::CardinalityViolation => sqlstate::CARDINALITY_VIOLATION,
            SqlError::SubqueryColumns(_)
            | SqlError::Syntax { .. }
            // PostgreSQL's type-name grammar, refusing in the same class as its statement
            // grammar: `'timestamp(-1)'::regtype` and `''::regtype` are both `42601`.
            | SqlError::TypeNameSyntax(_)
            | SqlError::TypeModifierNotAllowed(_)
            | SqlError::InvalidTypeName(_)
            | SqlError::InsertTooManyExpressions
            | SqlError::SyntaxAtOrNear(_) => sqlstate::SYNTAX_ERROR,
            SqlError::StatementTooComplex => sqlstate::STATEMENT_TOO_COMPLEX,
            SqlError::UndefinedTable(_)
            | SqlError::UndefinedTableForDrop(_)
            | SqlError::MissingFromEntry(_)
            | SqlError::ForwardCteReference(_)
            | SqlError::InvalidFromReference { .. } => sqlstate::UNDEFINED_TABLE,
            SqlError::DuplicateTableName(_) | SqlError::DuplicateCteName(_) => {
                sqlstate::DUPLICATE_ALIAS
            }
            SqlError::AmbiguousColumn(_) | SqlError::AmbiguousOrderBy(_) => {
                sqlstate::AMBIGUOUS_COLUMN
            }
            SqlError::UndefinedIndex(_)
            | SqlError::UndefinedType(_)
            | SqlError::UndefinedTrigger { .. } => sqlstate::UNDEFINED_OBJECT,
            SqlError::SystemCatalog(_) => sqlstate::INSUFFICIENT_PRIVILEGE,
            SqlError::WrongObjectType { .. } | SqlError::AlterActionOnWrongObject { .. } => {
                sqlstate::WRONG_OBJECT_TYPE
            }
            SqlError::UndefinedColumn(_)
            | SqlError::UndefinedColumnInForeignKey(_)
            | SqlError::UndefinedColumnInKey(_)
            | SqlError::UndefinedQualifiedColumn { .. }
            | SqlError::UsingColumnMissing { .. }
            | SqlError::UndefinedColumnInRelation { .. } => sqlstate::UNDEFINED_COLUMN,
            SqlError::DuplicateTable(_) | SqlError::AlreadyExistsSkipping(_) => {
                sqlstate::DUPLICATE_TABLE
            }
            SqlError::DuplicateColumn(_)
            | SqlError::DuplicateColumnInRelation { .. }
            | SqlError::DuplicateColumnSkipping { .. } => sqlstate::DUPLICATE_COLUMN,
            SqlError::UniqueViolation { .. } => sqlstate::UNIQUE_VIOLATION,
            SqlError::NotNullViolation(_) | SqlError::NotNullViolationInRelation { .. } => {
                sqlstate::NOT_NULL_VIOLATION
            }
            SqlError::InvalidTextRepresentation { .. } | SqlError::InvalidByteaFormat => {
                sqlstate::INVALID_TEXT_REPRESENTATION
            }
            SqlError::IntegerOutOfRange { .. }
            | SqlError::FloatOutOfRange { .. }
            | SqlError::IntegerLiteralOutOfRange(_)
            | SqlError::BigintOutOfRange
            | SqlError::NumericFieldOverflow { .. }
            | SqlError::SetvalOutOfBounds { .. } => sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            SqlError::InvalidDatetimeFormat { .. } => sqlstate::INVALID_DATETIME_FORMAT,
            SqlError::CannotCast { .. } => sqlstate::CANNOT_COERCE,
            SqlError::DatetimeFieldOutOfRange { .. } | SqlError::DatetimeOutOfRange { .. } => {
                sqlstate::DATETIME_FIELD_OVERFLOW
            }
            SqlError::TimeZoneDisplacementOutOfRange(_) => {
                sqlstate::INVALID_TIME_ZONE_DISPLACEMENT_VALUE
            }
            SqlError::InvalidHexDigit(_) | SqlError::OddHexDigits => {
                sqlstate::INVALID_PARAMETER_VALUE
            }
            SqlError::InvalidByteSequence(_) => sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            SqlError::DatatypeMismatch(_) | SqlError::DatatypeMismatchInColumn { .. } => {
                sqlstate::DATATYPE_MISMATCH
            }
            SqlError::UndefinedParameter(_) => sqlstate::UNDEFINED_PARAMETER,
            SqlError::NotImmutableInIndex => sqlstate::INVALID_OBJECT_DEFINITION,
            SqlError::UndefinedOperator { .. }
            | SqlError::UndefinedAggregate { .. }
            | SqlError::UndefinedFunction(_)
            | SqlError::UndefinedQualifiedFunction(_)
            | SqlError::UndefinedFunctionTypes(_)
            | SqlError::UndefinedFunctionName(_)
            | SqlError::UndefinedAggregateArity { .. } => sqlstate::UNDEFINED_FUNCTION,
            SqlError::ParameterlessAggregate => sqlstate::WRONG_OBJECT_TYPE,
            SqlError::GeneratedAlways { .. } => sqlstate::GENERATED_ALWAYS,
            SqlError::SequenceNotYetDefined(_) => sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            SqlError::NoSuchSavepoint(_) => sqlstate::NO_SUCH_SAVEPOINT,
            SqlError::GroupingError(_) | SqlError::AggregateNotAllowed(_) => {
                sqlstate::GROUPING_ERROR
            }
            SqlError::InvalidColumnReference(_) => sqlstate::INVALID_COLUMN_REFERENCE,
            SqlError::NegativeLimit("LIMIT") => sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
            SqlError::NegativeLimit(_) => sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
            SqlError::SerializationFailure { .. } => sqlstate::SERIALIZATION_FAILURE,
            SqlError::OutcomeUnknown(_) => sqlstate::STATEMENT_COMPLETION_UNKNOWN,
            SqlError::StoreUnavailable(_) => sqlstate::CONNECTION_FAILURE,
            SqlError::DoesNotExistSkipping { .. } => sqlstate::SUCCESSFUL_COMPLETION,
            SqlError::IdentifierTruncated { .. } => sqlstate::NAME_TOO_LONG,
            SqlError::InFailedTransaction => sqlstate::IN_FAILED_SQL_TRANSACTION,
            SqlError::ActiveTransaction
            | SqlError::SnapshotAfterQuery
            | SqlError::ConcurrentlyInTransactionBlock(_) => sqlstate::ACTIVE_SQL_TRANSACTION,
            SqlError::NoActiveTransaction
            | SqlError::SetTransactionOutsideBlock
            | SqlError::OutsideTransactionBlock(_) => sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            SqlError::CheckViolation { .. } => sqlstate::CHECK_VIOLATION,
            SqlError::ForeignKeyViolation { .. } | SqlError::ForeignKeyStillReferenced { .. } => {
                sqlstate::FOREIGN_KEY_VIOLATION
            }
            SqlError::NoUniqueConstraintForReference(_) => sqlstate::INVALID_FOREIGN_KEY,
            SqlError::DependentObjectsStillExist { .. } | SqlError::DependentTable { .. } => {
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST
            }
            SqlError::DuplicateConstraint { .. } | SqlError::DuplicateExtension(_) => {
                sqlstate::DUPLICATE_OBJECT
            }
            SqlError::StringDataRightTruncation(_) => sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            SqlError::UnsupportedUnicodeEscape => sqlstate::UNSUPPORTED_UNICODE_ESCAPE,
            // A declared length is `22023` too, which is not a family resemblance with the
            // parameter errors beside it — it is `anychar_typmodin` reaching for the same code.
            // Captured, both ends: `varchar(0)` and `varchar(10485761)`.
            SqlError::TypeLengthTooSmall(_)
            | SqlError::TypeLengthTooLarge(..)
            | SqlError::FloatPrecisionTooSmall
            | SqlError::FloatPrecisionTooLarge
            | SqlError::InvalidSnapshotIdentifier(_)
            | SqlError::InvalidParameterValue { .. }
            | SqlError::NonBooleanParameter(_)
            | SqlError::NumericPrecisionOutOfRange(_)
            | SqlError::NumericScaleOutOfRange(_)
            | SqlError::ParameterOutOfRange { .. } => sqlstate::INVALID_PARAMETER_VALUE,
            SqlError::CannotChangeParameter(_) => sqlstate::CANT_CHANGE_RUNTIME_PARAM,
            SqlError::SnapshotDoesNotExist(_) | SqlError::UnrecognizedParameter(_) => {
                sqlstate::UNDEFINED_OBJECT
            }
            SqlError::ReadOnlyTransaction(_) | SqlError::SchemaLeaseExpired { .. } => {
                sqlstate::READ_ONLY_SQL_TRANSACTION
            }
            SqlError::ConfigurationLimitExceeded(_) => sqlstate::CONFIGURATION_LIMIT_EXCEEDED,
            SqlError::ProtocolViolation(_) => sqlstate::PROTOCOL_VIOLATION,
            SqlError::InvalidSqlStatementName(_) => sqlstate::INVALID_SQL_STATEMENT_NAME,
            SqlError::InvalidCursorName(_) => sqlstate::INVALID_CURSOR_NAME,
            SqlError::InvalidPassword(_) => sqlstate::INVALID_PASSWORD,
            SqlError::DataCorrupted(_) => sqlstate::DATA_CORRUPTED,
            SqlError::Internal(_) => sqlstate::INTERNAL_ERROR,
        }
    }

    /// How the condition is reported.
    ///
    /// Only the two conditions PostgreSQL itself downgrades are not errors: `COMMIT` with no
    /// transaction open is a warning there, and copying that matters because `psql` scripts branch
    /// on whether the command failed.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            SqlError::AlreadyExistsSkipping(_)
            | SqlError::DoesNotExistSkipping { .. }
            | SqlError::DuplicateColumnSkipping { .. }
            | SqlError::IdentifierTruncated { .. } => Severity::Notice,
            SqlError::ActiveTransaction
            | SqlError::NoActiveTransaction
            | SqlError::SetTransactionOutsideBlock => Severity::Warning,
            SqlError::ProtocolViolation(_) | SqlError::InvalidPassword(_) => Severity::Fatal,
            _ => Severity::Error,
        }
    }

    /// The `DETAIL` field, when there is one.
    ///
    /// It is the part of a `23505` a user actually reads — the constraint name says *which* rule
    /// was broken and the detail says *what broke it*.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            SqlError::ForwardCteReference(name) => Some(format!(
                "There is a WITH item named \"{name}\", but it cannot be referenced from this \
                 part of the query."
            )),
            SqlError::UniqueViolation { key: Some(key), .. } => {
                Some(format!("{key} already exists."))
            }
            SqlError::NotNullViolationInRelation { row: Some(row), .. }
            | SqlError::CheckViolation { row, .. } => {
                Some(format!("Failing row contains ({row})."))
            }
            SqlError::NumericFieldOverflow { detail }
            | SqlError::ForeignKeyViolation { detail, .. }
            | SqlError::ForeignKeyStillReferenced { detail, .. }
            | SqlError::DependentTable { detail, .. } => Some(detail.clone()),
            SqlError::UndefinedOperator { .. } => {
                Some("No operator of that name accepts the given argument types.".to_owned())
            }
            SqlError::UndefinedAggregate { .. } | SqlError::UndefinedFunctionTypes(_) => {
                Some("No function of that name accepts the given argument types.".to_owned())
            }
            // The same sentence for the same condition: a function whose *name* exists and whose
            // arity does not. PostgreSQL says it for an aggregate and for `current_schema` alike,
            // which is why the two share it rather than each carrying a copy.
            SqlError::UndefinedFunctionName(_) => {
                Some("There is no function of that name.".to_owned())
            }
            SqlError::UndefinedAggregateArity { .. } | SqlError::UndefinedFunction(_) => {
                Some("No function of that name accepts the given number of arguments.".to_owned())
            }
            SqlError::GeneratedAlways { column } => Some(format!(
                "Column \"{column}\" is an identity column defined as GENERATED ALWAYS."
            )),
            SqlError::AlterActionOnWrongObject { kind, .. } => {
                Some(format!("This operation is not supported for {kind}."))
            }
            _ => None,
        }
    }

    /// The `HINT` field, when PostgreSQL sends one.
    ///
    /// A hint is not decoration: told `"t_b_key" is not a table`, a user's next question is what to
    /// do instead, and PostgreSQL answers it in the same message. Only the conditions where a real
    /// server was seen to send one have one here.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        match self {
            SqlError::ForwardCteReference(_) => Some(
                "Use WITH RECURSIVE, or re-order the WITH items to remove forward references."
                    .to_owned(),
            ),
            SqlError::Syntax { hint, .. } => hint.map(str::to_owned),
            // PostgreSQL's own, verbatim: the extension is missing from the *system*, not from the
            // statement, so the fix is outside SQL.
            SqlError::ExtensionNotAvailable(_) => Some(
                "The extension must first be installed on the system where PostgreSQL is running."
                    .to_owned(),
            ),
            // PostgreSQL owns `CHECKPOINT` for forcing a WAL checkpoint, so this node refuses it
            // by name (contract C2) and does not take the word for something else. A user who
            // wrote it was almost certainly reaching for a named checkpoint, which exists here.
            SqlError::FeatureNotSupported(feature)
                if feature == crate::parse::CHECKPOINT_FEATURE =>
            {
                Some(
                    "Esker names a timestamp with SELECT esker_checkpoint('<name>'). \
                     PostgreSQL's CHECKPOINT forces a WAL checkpoint and takes no name."
                        .to_owned(),
                )
            }
            SqlError::GeneratedAlways { .. } => Some("Use OVERRIDING SYSTEM VALUE to override.".to_owned()),
            SqlError::DatetimeFieldOutOfRange {
                datestyle_hint: true,
                ..
            } => Some("Perhaps you need a different \"DateStyle\" setting.".to_owned()),
            // The hint names a form this node does not have — `DROP … CASCADE` is `0A000` here —
            // and it is still the right sentence: it is what a real server says, and it is what
            // the user has to write once that unit lands. Saying something else would send them
            // looking for a different fix.
            SqlError::DependentTable { .. } => {
                Some("Use DROP ... CASCADE to drop the dependent objects too.".to_owned())
            }
            SqlError::WrongObjectType {
                found: "DROP INDEX",
                ..
            } => Some("Use DROP INDEX to remove an index.".to_owned()),
            SqlError::WrongObjectType {
                found: "DROP TABLE",
                ..
            } => Some("Use DROP TABLE to remove a table.".to_owned()),
            SqlError::WrongObjectType {
                found: "DROP SEQUENCE",
                ..
            } => Some("Use DROP SEQUENCE to remove a sequence.".to_owned()),
            SqlError::DependentObjectsStillExist { .. } => Some("You can drop the table instead.".to_owned()),
            SqlError::SchemaLeaseExpired { .. } => Some(
                "Reads are unaffected. Writes resume when this node can reach the placement driver."
                    .to_owned(),
            ),
            SqlError::DatatypeMismatchInColumn { .. } => {
                Some("You will need to rewrite or cast the expression.".to_owned())
            }
            // The same hint a real server sends with the same `42883`, word for word, for an
            // operator and for an aggregate alike.
            SqlError::UndefinedOperator { .. }
            | SqlError::UndefinedAggregate { .. }
            | SqlError::UndefinedFunctionTypes(_) => {
                Some("You might need to add explicit type casts.".to_owned())
            }
            // PostgreSQL lists the values an enum parameter takes, and the list is the parameter's
            // rather than the error's — looked up so the two can never say different things.
            SqlError::InvalidParameterValue { name, .. } => match crate::parameter::lookup(name) {
                Ok(crate::parameter::Parameter {
                    values: crate::parameter::Values::Enum(allowed),
                    ..
                }) => Some(format!("Available values: {}.", allowed.join(", "))),
                _ => None,
            },
            // PostgreSQL names the alias that took the name away, which is the whole of what a
            // user needs: the table is there, under a name they did not write.
            SqlError::InvalidFromReference { alias, .. } => Some(format!(
                "Perhaps you meant to reference the table alias \"{alias}\"."
            )),
            _ => None,
        }
    }

    /// The one-based character offset PostgreSQL reports in the `P` field, when there is one.
    #[must_use]
    pub fn position(&self) -> Option<u32> {
        match self {
            SqlError::Syntax { position, .. } => *position,
            _ => None,
        }
    }

    /// Contract C2's constructor. Takes the feature's name as it should appear to the client —
    /// PostgreSQL names the construct, not the module that refused it.
    pub fn unsupported(feature: impl Into<String>) -> Self {
        SqlError::FeatureNotSupported(feature.into())
    }

    /// True when the condition aborts an open transaction block. A warning does not.
    #[must_use]
    pub fn aborts_transaction(&self) -> bool {
        matches!(self.severity(), Severity::Error | Severity::Fatal)
    }
}

/// The row codec's failures, mapped to the conditions a client is told about.
///
/// Three variants and three destinations, and the mapping is the reason `esker_keys::row` has
/// three rather than one. Corruption is `DataCorrupted`; a mismatch is `Internal`, because only a
/// bug above this layer produces one; and invalid UTF-8 keeps PostgreSQL's own `22021`, with the
/// offending byte, because that one is a condition a *user* can cause and a client reads the
/// message. Collapsing them here would lose two sqlstates
/// ([ADR 0029](../../docs/adr/0030-the-row-codec-moves-down.md)).
impl From<esker_keys::row::RowError> for SqlError {
    fn from(error: esker_keys::row::RowError) -> Self {
        use esker_keys::row::RowError;
        match error {
            RowError::Corrupt(what) => SqlError::DataCorrupted(what),
            RowError::Mismatch(what) => SqlError::Internal(what),
            RowError::InvalidUtf8(byte) => SqlError::InvalidByteSequence(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Severity, SqlError};
    use crate::sqlstate;

    /// Contract C2: the message must name the feature, because "not supported" on its own tells a
    /// user nothing about what to change.
    #[test]
    fn unsupported_names_the_feature_and_uses_0a000() {
        let error = SqlError::unsupported("JOIN");
        assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(error.to_string(), "JOIN is not supported");
        assert_eq!(error.severity(), Severity::Error);
        assert!(error.aborts_transaction());
    }

    /// PostgreSQL reports `COMMIT` outside a transaction as a warning and carries on. A `psql`
    /// script that treats it as failure would behave differently against us if we escalated it.
    #[test]
    fn committing_outside_a_transaction_is_a_warning_that_does_not_abort() {
        let error = SqlError::NoActiveTransaction;
        assert_eq!(error.severity(), Severity::Warning);
        assert!(!error.aborts_transaction());
        assert_eq!(error.sqlstate(), sqlstate::NO_ACTIVE_SQL_TRANSACTION);
    }

    /// These strings go on the wire verbatim; a client comparing against "ERROR" must match.
    #[test]
    fn severity_tokens_are_the_ones_postgresql_sends() {
        assert_eq!(Severity::Error.as_str(), "ERROR");
        assert_eq!(Severity::Warning.as_str(), "WARNING");
        assert_eq!(Severity::Notice.as_str(), "NOTICE");
        assert_eq!(Severity::Fatal.as_str(), "FATAL");
    }

    /// The message text is part of the compatibility surface for the conditions users read.
    #[test]
    fn messages_read_the_way_postgresqls_do() {
        assert_eq!(
            SqlError::UndefinedTable("accounts".into()).to_string(),
            "relation \"accounts\" does not exist"
        );
        assert_eq!(
            SqlError::UniqueViolation {
                constraint: "accounts_pkey".into(),
                key: None,
            }
            .to_string(),
            "duplicate key value violates unique constraint \"accounts_pkey\""
        );
        assert_eq!(
            SqlError::InFailedTransaction.to_string(),
            "current transaction is aborted, commands ignored until end of transaction block"
        );
        assert_eq!(
            SqlError::StatementTooComplex.to_string(),
            "stack depth limit exceeded"
        );
    }
}
