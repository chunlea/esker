//! SQLSTATE codes, named after PostgreSQL's own condition names.
//!
//! Every error this crate reports on the wire carries one of these five-character codes, and
//! contract C3 in `docs/plans/phase-6a.md` makes the choice of code part of the compatibility
//! promise: a client that branches on `23505` must see `23505` from us in exactly the cases it
//! would see it from PostgreSQL. The names come from PostgreSQL's `src/backend/utils/errcodes.txt`
//! so that a reader can grep for the same string in both projects.
//!
//! The first two characters are the class; a client that does not recognise a code is expected to
//! fall back to the class, which is why an unrecognised condition must never be reported as
//! `XX000` when a real class exists for it.

/// A SQLSTATE is always exactly five characters.
pub const LEN: usize = 5;

// --- Class 00 — Successful Completion ---

/// Not an error. It rides on a `NoticeResponse`, which is how PostgreSQL says
/// `table "t" does not exist, skipping` for a `DROP TABLE IF EXISTS` that did nothing — the
/// statement succeeded and the notice is the whole of what happened. Captured, because the
/// symmetric notice (`relation "t" already exists, skipping`) carries `42P07` instead, and the
/// asymmetry is not something a reading would have produced.
pub const SUCCESSFUL_COMPLETION: &str = "00000";

// --- Class 08 — Connection Exception ---

/// The frontend sent something the protocol does not allow.
pub const PROTOCOL_VIOLATION: &str = "08P01";

// --- Class 0A — Feature Not Supported ---

/// Parsed, understood, and deliberately not executed. Contract C2's code: every statement Esker
/// can read but cannot run comes back as this, naming the feature.
pub const FEATURE_NOT_SUPPORTED: &str = "0A000";

// --- Class 21 — Cardinality Violation ---

/// A subquery used where one value goes returned more than one row.
///
/// The whole of class 21, and it is a class of its own rather than a data exception because the
/// wrong thing is the *number of rows*, not the values in them: PostgreSQL raises it while the
/// statement runs and only when the rows are actually there, so the same statement can succeed on
/// one snapshot and raise on the next (`docs/plans/phase-12-subquery.md` §1).
pub const CARDINALITY_VIOLATION: &str = "21000";

/// `2201B invalid_regular_expression` — a pattern `~` cannot compile. One code, and the *message*
/// is what says which thing is malformed.
pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";

// --- Class 22 — Data Exception ---

/// A literal could not be read as its target type — `'abc'::int8`.
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
/// A value is outside its type's range.
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";

/// `invalid_argument_for_power_function` — `(-2) ^ 0.5`, whose answer is not a real number.
pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
/// Division by zero, including modulo.
pub const DIVISION_BY_ZERO: &str = "22012";
/// A datetime literal PostgreSQL's own parser would also refuse — `'abc'::timestamptz`. Note that
/// this is *not* `22P02`: the datetime types have their own condition, and a client that branches
/// on the code would see the difference.
pub const INVALID_DATETIME_FORMAT: &str = "22007";
/// A cast between two types that have none. Not a *failed* cast — that is the value's own error —
/// but a pair for which no cast exists at all, which PostgreSQL decides before it reads a value.
pub const CANNOT_COERCE: &str = "42846";
/// A datetime field is out of range — a thirteenth month, or an instant past the type's end.
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";

/// One field of an interval past its own width, which is a **different** code from the whole
/// value overflowing: `'2147483648 months'` is this and `'178956971 years'` is `22008`.
pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
/// A time zone displacement past `±15:59`, which is its own condition and not a field overflow.
pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
/// A value longer than the length its column declared: `varchar(5)` given six characters.
///
/// PostgreSQL's own name for it is `string_data_right_truncation`, which describes what the
/// standard says should happen and not what PostgreSQL does — it raises rather than truncates,
/// and an explicit `::varchar(5)` cast is the one place it really does truncate.
pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
/// A `jsonb` document containing a NUL escape, which its text form cannot hold.
pub const UNSUPPORTED_UNICODE_ESCAPE: &str = "22P05";
/// Bytes that are not valid in the server encoding.
pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
/// What `bytea`'s hexadecimal input reports a bad digit or an odd count with. Surprising — the
/// neighbouring failures in the same input function are `22P02` — and captured, not assumed.
pub const INVALID_PARAMETER_VALUE: &str = "22023";

/// `indeterminate_datatype` — `ARRAY[]` with nothing to say what it is an array of.
pub const INDETERMINATE_DATATYPE: &str = "42P18";
/// A `SET` of a parameter that exists and is fixed — a different answer from one that does not
/// exist, which is [`UNDEFINED_OBJECT`].
pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";

/// A negative `LIMIT`.
pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201W";
/// A negative `OFFSET`. Its own code, not the same one — captured, because collapsing them would
/// tell a client the wrong clause was wrong.
pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: &str = "2201X";

// --- Class 23 — Integrity Constraint Violation ---

/// A NULL reached a `NOT NULL` column.
pub const NOT_NULL_VIOLATION: &str = "23502";
/// A duplicate reached a `PRIMARY KEY` or `UNIQUE` index.
pub const UNIQUE_VIOLATION: &str = "23505";
/// A row a `CHECK` constraint refuses.
pub const CHECK_VIOLATION: &str = "23514";
/// Class 23 — a row two `EXCLUDE` keys conflict over. **Not `23505`**: a unique index refuses an
/// equal key, an exclusion constraint refuses one an operator relates, and PostgreSQL keeps them
/// apart so a client can tell which kind of index it hit.
pub const EXCLUSION_VIOLATION: &str = "23P01";
/// A row that points at a parent row that is not there, and a parent row something still points
/// at. **One code for both directions**, which is why the two messages differ so much: the second
/// names the child table as well as the parent.
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
/// A `FOREIGN KEY` whose referenced columns are not a key of the parent.
pub const INVALID_FOREIGN_KEY: &str = "42830";

// --- Class 25 — Invalid Transaction State ---

/// `BEGIN` inside a transaction block. PostgreSQL warns and continues rather than failing, so this
/// is here for the cases where the state really is invalid.
pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
/// `COMMIT` or `ROLLBACK` with no transaction block open.
pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
/// A write in a transaction that may not write — which here is every transaction reading the past
/// (`docs/adr/0021-time-machine.md` Decision 1).
pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
/// Any statement after an error inside a transaction block, until `ROLLBACK`. Half of contract
/// C2's state-machine promise lives on this code.
pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
/// A session that sat idle inside a transaction block for longer than
/// `idle_in_transaction_session_timeout`, and is being **terminated** for it.
///
/// The one condition in this module whose severity is `FATAL` rather than `ERROR`: it does not
/// fail a statement, it ends the connection, and a client that treats it as a statement error
/// waits for a server that has gone (`crate::pgwire::server::Connection::run`).
pub const IDLE_IN_TRANSACTION_SESSION_TIMEOUT: &str = "25P03";

// --- Class 26 — Invalid SQL Statement Name ---

/// `Bind`/`Describe`/`Execute` naming a prepared statement that was never parsed.
pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";

// --- Class 28 — Invalid Authorization Specification ---

/// Authentication failed.
pub const INVALID_PASSWORD: &str = "28P01";
/// The role does not exist, or is not permitted to connect.
pub const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";

// --- Class 2B — Dependent Privilege/Object Still Exist ---

/// Something else needs the object being dropped — `DROP INDEX t_pkey` when a primary key
/// constraint is what put that index there.
pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";

/// `42P06 duplicate_schema` — `CREATE SCHEMA` over one that is there.
pub const DUPLICATE_SCHEMA: &str = "42P06";

/// `3F000 invalid_schema_name` — a schema that is not there.
///
/// **Its own class**, not `42P01`: `CREATE TABLE nosuchschema.t` fails on the *schema* and gives
/// this, where a relation missing from a schema that exists gives `42P01`. Measured, both.
pub const INVALID_SCHEMA_NAME: &str = "3F000";

// --- Class 34 — Invalid Cursor Name ---

/// `Bind`/`Execute`/`Close` naming a portal that does not exist.
pub const INVALID_CURSOR_NAME: &str = "34000";

// --- Class 3D — Invalid Catalog Name ---

/// The startup packet asked for a database that does not exist.
pub const INVALID_CATALOG_NAME: &str = "3D000";

// --- Class 40 — Transaction Rollback ---

/// Two transactions wrote the same key and this one lost. Percolator detects it at prewrite, and
/// this is the code PostgreSQL uses for the same situation under serializable isolation — a
/// client is expected to see it and retry.
pub const SERIALIZATION_FAILURE: &str = "40001";
/// A request went out and no usable answer came back, so whether it was applied is unknown. The
/// one condition a distributed store has that a single-process one does not, and PostgreSQL has a
/// code for it because two-phase commit has the same problem.
pub const STATEMENT_COMPLETION_UNKNOWN: &str = "40003";
/// The store could not be reached, or would not answer in time.
pub const CONNECTION_FAILURE: &str = "08006";

// --- Class 42 — Syntax Error or Access Rule Violation ---

/// `42501` — the statement is refused because of what it is being done *to*: a write to a
/// `pg_catalog` relation. Measured on 19beta1, where `DROP TABLE pg_type` answers
/// `permission denied: "pg_type" is a system catalog`.
pub const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// The statement is not valid SQL. Contract C1 says this must never be the answer to a statement
/// PostgreSQL 19 would have accepted.
pub const SYNTAX_ERROR: &str = "42601";
/// A column name that resolves to nothing.
pub const UNDEFINED_COLUMN: &str = "42703";
/// A table name that resolves to nothing.
pub const UNDEFINED_TABLE: &str = "42P01";
/// A bare column name that more than one table in the query has.
pub const AMBIGUOUS_COLUMN: &str = "42702";
/// A call whose argument types match more than one candidate: `sum('lit')`, where the `unknown`
/// literal fits every `sum` PostgreSQL has.
pub const AMBIGUOUS_FUNCTION: &str = "42725";
/// Two FROM entries under one name — `FROM t JOIN t`, or two aliases spelled the same.
pub const DUPLICATE_ALIAS: &str = "42712";
/// `CREATE TABLE` for a name that already exists.
pub const DUPLICATE_TABLE: &str = "42P07";
/// Two columns of one table share a name.
pub const DUPLICATE_COLUMN: &str = "42701";
/// `CREATE INDEX` for a name that already exists.
pub const DUPLICATE_OBJECT: &str = "42710";
/// An index name that resolves to nothing.
pub const UNDEFINED_OBJECT: &str = "42704";
/// A name that exists and is the wrong kind of thing — `DROP TABLE` naming an index. Not
/// `42P01`: the object is there, it is just not what the statement can act on. Captured, because
/// collapsing the two would tell a user their index does not exist.
pub const WRONG_OBJECT_TYPE: &str = "42809";
/// An operator or function applied to types it is not defined for.
pub const DATATYPE_MISMATCH: &str = "42804";
/// No such function.
pub const UNDEFINED_FUNCTION: &str = "42883";
/// `ROLLBACK TO` or `RELEASE` naming a savepoint that is not there. Its own class, 3B, which has
/// this one condition in it.
pub const NO_SUCH_SAVEPOINT: &str = "3B001";
/// `currval` before this session has called `nextval`. PostgreSQL's class 55, and a statement
/// about the *session* rather than about the sequence — which does have a value.
pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
/// A value written into a `GENERATED ALWAYS AS IDENTITY` column. PostgreSQL's own class 428,
/// which has exactly this one condition in it.
pub const GENERATED_ALWAYS: &str = "428C9";
/// A column that is neither a grouping key nor inside an aggregate, and an aggregate written in a
/// clause evaluated before the groups exist. One condition, because PostgreSQL gives them one
/// code: `WHERE count(*) > 1` and `SELECT n FROM t GROUP BY g` are both `42803`.
pub const GROUPING_ERROR: &str = "42803";
/// `ORDER BY`, `GROUP BY` or `SELECT DISTINCT` naming something the target list does not have —
/// a position out of range, or a `DISTINCT` sort key that is not selected.
pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
/// A `$1` with nothing bound to it.
pub const UNDEFINED_PARAMETER: &str = "42P02";
/// A definition that cannot be what it claims: an index expression whose value is not a function
/// of the row alone. PostgreSQL's `invalid_object_definition`, and the code it gives for
/// `CREATE INDEX ON t ((now()))`.
pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
/// A table definition that cannot be built — no primary key, in our case.
pub const INVALID_TABLE_DEFINITION: &str = "42P16";
/// An identifier longer than 63 bytes. A *notice*, not an error: PostgreSQL truncates and carries
/// on, so a client that treated this as a failure would disagree with every other one.
pub const NAME_TOO_LONG: &str = "42622";

// --- Class 53 — Insufficient Resources ---

/// A query needs more of a bounded resource than the node will give it — the v1 in-memory sort
/// limit is reported here rather than by running out of memory.
pub const CONFIGURATION_LIMIT_EXCEEDED: &str = "53400";

// --- Class 54 — Program Limit Exceeded ---

/// The statement nests deeper than the parser may safely descend. PostgreSQL raises this when
/// `max_stack_depth` is exceeded; `crate::parse`'s depth guard raises it for the same reason
/// (`docs/adr/0014-sqlparser.md`).
pub const STATEMENT_TOO_COMPLEX: &str = "54001";
/// A row has more columns than the node will build.
pub const TOO_MANY_COLUMNS: &str = "54011";

// --- Class XX — Internal Error ---

/// A bug here, not a mistake there. Nothing reachable from user input may report this.
pub const INTERNAL_ERROR: &str = "XX000";
/// Bytes came back from storage that this node cannot read as what they should be. Never a panic
/// and never silently skipped (`CLAUDE.md` invariant 2).
pub const DATA_CORRUPTED: &str = "XX001";

#[cfg(test)]
mod tests {
    /// Every constant in this module, so the shape tests below cannot silently miss one.
    const ALL: &[(&str, &str)] = &[
        ("PROTOCOL_VIOLATION", super::PROTOCOL_VIOLATION),
        ("FEATURE_NOT_SUPPORTED", super::FEATURE_NOT_SUPPORTED),
        (
            "INVALID_TEXT_REPRESENTATION",
            super::INVALID_TEXT_REPRESENTATION,
        ),
        (
            "NUMERIC_VALUE_OUT_OF_RANGE",
            super::NUMERIC_VALUE_OUT_OF_RANGE,
        ),
        ("DIVISION_BY_ZERO", super::DIVISION_BY_ZERO),
        ("INVALID_DATETIME_FORMAT", super::INVALID_DATETIME_FORMAT),
        ("DATETIME_FIELD_OVERFLOW", super::DATETIME_FIELD_OVERFLOW),
        ("INTERVAL_FIELD_OVERFLOW", super::INTERVAL_FIELD_OVERFLOW),
        (
            "INVALID_TIME_ZONE_DISPLACEMENT_VALUE",
            super::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
        ),
        (
            "CHARACTER_NOT_IN_REPERTOIRE",
            super::CHARACTER_NOT_IN_REPERTOIRE,
        ),
        ("INVALID_PARAMETER_VALUE", super::INVALID_PARAMETER_VALUE),
        (
            "CANT_CHANGE_RUNTIME_PARAM",
            super::CANT_CHANGE_RUNTIME_PARAM,
        ),
        (
            "INVALID_ROW_COUNT_IN_LIMIT_CLAUSE",
            super::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
        ),
        (
            "INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE",
            super::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
        ),
        ("NOT_NULL_VIOLATION", super::NOT_NULL_VIOLATION),
        ("SERIALIZATION_FAILURE", super::SERIALIZATION_FAILURE),
        (
            "STATEMENT_COMPLETION_UNKNOWN",
            super::STATEMENT_COMPLETION_UNKNOWN,
        ),
        ("CONNECTION_FAILURE", super::CONNECTION_FAILURE),
        ("UNIQUE_VIOLATION", super::UNIQUE_VIOLATION),
        ("ACTIVE_SQL_TRANSACTION", super::ACTIVE_SQL_TRANSACTION),
        (
            "NO_ACTIVE_SQL_TRANSACTION",
            super::NO_ACTIVE_SQL_TRANSACTION,
        ),
        (
            "READ_ONLY_SQL_TRANSACTION",
            super::READ_ONLY_SQL_TRANSACTION,
        ),
        (
            "IN_FAILED_SQL_TRANSACTION",
            super::IN_FAILED_SQL_TRANSACTION,
        ),
        (
            "IDLE_IN_TRANSACTION_SESSION_TIMEOUT",
            super::IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
        ),
        (
            "INVALID_SQL_STATEMENT_NAME",
            super::INVALID_SQL_STATEMENT_NAME,
        ),
        ("INVALID_PASSWORD", super::INVALID_PASSWORD),
        (
            "INVALID_AUTHORIZATION_SPECIFICATION",
            super::INVALID_AUTHORIZATION_SPECIFICATION,
        ),
        ("INVALID_CURSOR_NAME", super::INVALID_CURSOR_NAME),
        (
            "DEPENDENT_OBJECTS_STILL_EXIST",
            super::DEPENDENT_OBJECTS_STILL_EXIST,
        ),
        ("DUPLICATE_SCHEMA", super::DUPLICATE_SCHEMA),
        ("INVALID_SCHEMA_NAME", super::INVALID_SCHEMA_NAME),
        ("INVALID_CATALOG_NAME", super::INVALID_CATALOG_NAME),
        ("CARDINALITY_VIOLATION", super::CARDINALITY_VIOLATION),
        (
            "INVALID_REGULAR_EXPRESSION",
            super::INVALID_REGULAR_EXPRESSION,
        ),
        ("SYNTAX_ERROR", super::SYNTAX_ERROR),
        ("UNDEFINED_COLUMN", super::UNDEFINED_COLUMN),
        ("UNDEFINED_TABLE", super::UNDEFINED_TABLE),
        ("AMBIGUOUS_COLUMN", super::AMBIGUOUS_COLUMN),
        ("AMBIGUOUS_FUNCTION", super::AMBIGUOUS_FUNCTION),
        ("DUPLICATE_ALIAS", super::DUPLICATE_ALIAS),
        ("DUPLICATE_TABLE", super::DUPLICATE_TABLE),
        ("DUPLICATE_COLUMN", super::DUPLICATE_COLUMN),
        ("DUPLICATE_OBJECT", super::DUPLICATE_OBJECT),
        ("UNDEFINED_OBJECT", super::UNDEFINED_OBJECT),
        ("WRONG_OBJECT_TYPE", super::WRONG_OBJECT_TYPE),
        ("DATATYPE_MISMATCH", super::DATATYPE_MISMATCH),
        ("UNDEFINED_FUNCTION", super::UNDEFINED_FUNCTION),
        ("GENERATED_ALWAYS", super::GENERATED_ALWAYS),
        ("NO_SUCH_SAVEPOINT", super::NO_SUCH_SAVEPOINT),
        (
            "OBJECT_NOT_IN_PREREQUISITE_STATE",
            super::OBJECT_NOT_IN_PREREQUISITE_STATE,
        ),
        ("GROUPING_ERROR", super::GROUPING_ERROR),
        ("INVALID_COLUMN_REFERENCE", super::INVALID_COLUMN_REFERENCE),
        ("INVALID_TABLE_DEFINITION", super::INVALID_TABLE_DEFINITION),
        ("NAME_TOO_LONG", super::NAME_TOO_LONG),
        ("UNDEFINED_PARAMETER", super::UNDEFINED_PARAMETER),
        ("SUCCESSFUL_COMPLETION", super::SUCCESSFUL_COMPLETION),
        (
            "CONFIGURATION_LIMIT_EXCEEDED",
            super::CONFIGURATION_LIMIT_EXCEEDED,
        ),
        ("STATEMENT_TOO_COMPLEX", super::STATEMENT_TOO_COMPLEX),
        ("TOO_MANY_COLUMNS", super::TOO_MANY_COLUMNS),
        ("INTERNAL_ERROR", super::INTERNAL_ERROR),
        ("DATA_CORRUPTED", super::DATA_CORRUPTED),
    ];

    /// The wire format gives the code field no length prefix, so a code of the wrong width would
    /// corrupt every field after it in the `ErrorResponse`.
    #[test]
    fn every_code_is_five_uppercase_alphanumerics() {
        for (name, code) in ALL {
            assert_eq!(code.len(), super::LEN, "{name} is not five characters");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()),
                "{name} = {code} is not upper-case alphanumeric"
            );
        }
    }

    /// Two conditions sharing a code means a client cannot tell them apart — which is the entire
    /// purpose of the code.
    #[test]
    fn no_two_conditions_share_a_code() {
        let mut seen = std::collections::BTreeMap::new();
        for (name, code) in ALL {
            if let Some(previous) = seen.insert(*code, *name) {
                panic!("{name} and {previous} both use {code}");
            }
        }
    }

    /// Class 00 means success. An error that reported one would be read as "no error".
    ///
    /// `SUCCESSFUL_COMPLETION` is the one code that is *meant* to be in it, because PostgreSQL
    /// really does send `00000` on a `NoticeResponse` for a `DROP ... IF EXISTS` that skipped. It
    /// is exempt by name rather than by omission, so the exemption is visible.
    #[test]
    fn no_error_is_in_the_success_class() {
        for (name, code) in ALL {
            if *name == "SUCCESSFUL_COMPLETION" {
                continue;
            }
            assert_ne!(&code[..2], "00", "{name} is in the success class");
        }
    }
}
