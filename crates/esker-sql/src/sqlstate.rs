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

// --- Class 22 — Data Exception ---

/// A literal could not be read as its target type — `'abc'::int8`.
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
/// A value is outside its type's range.
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
/// Division by zero, including modulo.
pub const DIVISION_BY_ZERO: &str = "22012";
/// A datetime literal PostgreSQL's own parser would also refuse — `'abc'::timestamptz`. Note that
/// this is *not* `22P02`: the datetime types have their own condition, and a client that branches
/// on the code would see the difference.
pub const INVALID_DATETIME_FORMAT: &str = "22007";
/// A datetime field is out of range — a thirteenth month, or an instant past the type's end.
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
/// A time zone displacement past `±15:59`, which is its own condition and not a field overflow.
pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
/// Bytes that are not valid in the server encoding.
pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
/// What `bytea`'s hexadecimal input reports a bad digit or an odd count with. Surprising — the
/// neighbouring failures in the same input function are `22P02` — and captured, not assumed.
pub const INVALID_PARAMETER_VALUE: &str = "22023";

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

// --- Class 25 — Invalid Transaction State ---

/// `BEGIN` inside a transaction block. PostgreSQL warns and continues rather than failing, so this
/// is here for the cases where the state really is invalid.
pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
/// `COMMIT` or `ROLLBACK` with no transaction block open.
pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
/// Any statement after an error inside a transaction block, until `ROLLBACK`. Half of contract
/// C2's state-machine promise lives on this code.
pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";

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

// --- Class 42 — Syntax Error or Access Rule Violation ---

/// The statement is not valid SQL. Contract C1 says this must never be the answer to a statement
/// PostgreSQL 19 would have accepted.
pub const SYNTAX_ERROR: &str = "42601";
/// A column name that resolves to nothing.
pub const UNDEFINED_COLUMN: &str = "42703";
/// A table name that resolves to nothing.
pub const UNDEFINED_TABLE: &str = "42P01";
/// A bare column name that more than one table in the query has.
pub const AMBIGUOUS_COLUMN: &str = "42702";
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
/// A `$1` with nothing bound to it.
pub const UNDEFINED_PARAMETER: &str = "42P02";
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
            "INVALID_ROW_COUNT_IN_LIMIT_CLAUSE",
            super::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
        ),
        (
            "INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE",
            super::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
        ),
        ("NOT_NULL_VIOLATION", super::NOT_NULL_VIOLATION),
        ("SERIALIZATION_FAILURE", super::SERIALIZATION_FAILURE),
        ("UNIQUE_VIOLATION", super::UNIQUE_VIOLATION),
        ("ACTIVE_SQL_TRANSACTION", super::ACTIVE_SQL_TRANSACTION),
        (
            "NO_ACTIVE_SQL_TRANSACTION",
            super::NO_ACTIVE_SQL_TRANSACTION,
        ),
        (
            "IN_FAILED_SQL_TRANSACTION",
            super::IN_FAILED_SQL_TRANSACTION,
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
        ("INVALID_CATALOG_NAME", super::INVALID_CATALOG_NAME),
        ("SYNTAX_ERROR", super::SYNTAX_ERROR),
        ("UNDEFINED_COLUMN", super::UNDEFINED_COLUMN),
        ("UNDEFINED_TABLE", super::UNDEFINED_TABLE),
        ("AMBIGUOUS_COLUMN", super::AMBIGUOUS_COLUMN),
        ("DUPLICATE_TABLE", super::DUPLICATE_TABLE),
        ("DUPLICATE_COLUMN", super::DUPLICATE_COLUMN),
        ("DUPLICATE_OBJECT", super::DUPLICATE_OBJECT),
        ("UNDEFINED_OBJECT", super::UNDEFINED_OBJECT),
        ("WRONG_OBJECT_TYPE", super::WRONG_OBJECT_TYPE),
        ("DATATYPE_MISMATCH", super::DATATYPE_MISMATCH),
        ("UNDEFINED_FUNCTION", super::UNDEFINED_FUNCTION),
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
