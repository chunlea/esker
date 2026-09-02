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

    /// A datetime field is outside its own range — a thirteenth month, a thirtieth of February.
    #[error("date/time field value out of range: \"{0}\"")]
    DatetimeFieldOutOfRange(String),

    /// Every field was in range and the instant they name is not: past 294276 AD, or before
    /// 4714 BC.
    #[error("timestamp out of range: \"{0}\"")]
    TimestampOutOfRange(String),

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
    #[error("syntax error at or near \"{0}\"")]
    SyntaxAtOrNear(&'static str),

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

    /// A type name that names no type on this node: `42704`.
    ///
    /// What `'nope'::regtype` answers on a real server, word for word. It is also what this node
    /// answers for a type PostgreSQL *has* and it does not — `numeric`, an array — which is a
    /// declared divergence rather than an oversight: answering `1700` would hand a client the OID
    /// of a type this node can neither store nor send, and `crates/esker-sql/src/catalog/
    /// pg_catalog.rs` already refuses to list one for exactly that reason.
    #[error("type \"{0}\" does not exist")]
    UndefinedType(String),

    /// A value longer than its column's declared length: `22001`.
    ///
    /// The type is spelled as `format_type` writes it — `character varying(5)`, `character(3)` —
    /// and **not** the way the two messages below spell it. PostgreSQL really does use two
    /// vocabularies for one type, `tests/corpus/pg19_typmod.txt` has both, and neither was
    /// guessed.
    #[error("value too long for type {0}")]
    StringDataRightTruncation(String),

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
            SqlError::FeatureNotSupported(_) | SqlError::SnapshotIsolationRequired => {
                sqlstate::FEATURE_NOT_SUPPORTED
            }
            SqlError::Syntax { .. }
            | SqlError::InsertTooManyExpressions
            | SqlError::SyntaxAtOrNear(_) => sqlstate::SYNTAX_ERROR,
            SqlError::StatementTooComplex => sqlstate::STATEMENT_TOO_COMPLEX,
            SqlError::UndefinedTable(_)
            | SqlError::UndefinedTableForDrop(_)
            | SqlError::MissingFromEntry(_)
            | SqlError::InvalidFromReference { .. } => sqlstate::UNDEFINED_TABLE,
            SqlError::DuplicateTableName(_) => sqlstate::DUPLICATE_ALIAS,
            SqlError::AmbiguousColumn(_) | SqlError::AmbiguousOrderBy(_) => {
                sqlstate::AMBIGUOUS_COLUMN
            }
            SqlError::UndefinedIndex(_) | SqlError::UndefinedType(_) => sqlstate::UNDEFINED_OBJECT,
            SqlError::SystemCatalog(_) => sqlstate::INSUFFICIENT_PRIVILEGE,
            SqlError::DependentObjectsStillExist { .. } => sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            SqlError::WrongObjectType { .. } | SqlError::AlterActionOnWrongObject { .. } => {
                sqlstate::WRONG_OBJECT_TYPE
            }
            SqlError::UndefinedColumn(_)
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
            | SqlError::SetvalOutOfBounds { .. } => sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            SqlError::InvalidDatetimeFormat { .. } => sqlstate::INVALID_DATETIME_FORMAT,
            SqlError::DatetimeFieldOutOfRange(_) | SqlError::TimestampOutOfRange(_) => {
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
            SqlError::UndefinedOperator { .. }
            | SqlError::UndefinedAggregate { .. }
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
            SqlError::StringDataRightTruncation(_) => sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            // A declared length is `22023` too, which is not a family resemblance with the
            // parameter errors beside it — it is `anychar_typmodin` reaching for the same code.
            // Captured, both ends: `varchar(0)` and `varchar(10485761)`.
            SqlError::TypeLengthTooSmall(_)
            | SqlError::TypeLengthTooLarge(..)
            | SqlError::InvalidSnapshotIdentifier(_)
            | SqlError::InvalidParameterValue { .. }
            | SqlError::NonBooleanParameter(_)
            | SqlError::ParameterOutOfRange { .. } => sqlstate::INVALID_PARAMETER_VALUE,
            SqlError::CannotChangeParameter(_) => sqlstate::CANT_CHANGE_RUNTIME_PARAM,
            SqlError::NonStandardStringLiterals => sqlstate::FEATURE_NOT_SUPPORTED,
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
            SqlError::UniqueViolation { key: Some(key), .. } => {
                Some(format!("{key} already exists."))
            }
            SqlError::NotNullViolationInRelation { row: Some(row), .. } => {
                Some(format!("Failing row contains ({row})."))
            }
            SqlError::UndefinedOperator { .. } => {
                Some("No operator of that name accepts the given argument types.".to_owned())
            }
            SqlError::UndefinedAggregate { .. } => {
                Some("No function of that name accepts the given argument types.".to_owned())
            }
            SqlError::UndefinedAggregateArity { .. } => {
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
            SqlError::Syntax { hint, .. } => hint.map(str::to_owned),
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
            SqlError::UndefinedOperator { .. } | SqlError::UndefinedAggregate { .. } => {
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
