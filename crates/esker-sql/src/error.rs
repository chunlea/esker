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
    },

    /// The statement nests deeper than the parser may safely descend
    /// (`crate::parse::MAX_NESTING_DEPTH`). PostgreSQL raises the same condition when
    /// `max_stack_depth` is exceeded.
    #[error("stack depth limit exceeded")]
    StatementTooComplex,

    /// No such table.
    #[error("relation \"{0}\" does not exist")]
    UndefinedTable(String),

    /// No such column.
    #[error("column \"{0}\" does not exist")]
    UndefinedColumn(String),

    /// `CREATE TABLE` over a live name.
    #[error("relation \"{0}\" already exists")]
    DuplicateTable(String),

    /// Two columns of one table share a name.
    #[error("column \"{0}\" specified more than once")]
    DuplicateColumn(String),

    /// A duplicate reached a unique index.
    #[error("duplicate key value violates unique constraint \"{0}\"")]
    UniqueViolation(String),

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

    /// An operator or function met types it is not defined for.
    #[error("{0}")]
    DatatypeMismatch(String),

    /// Two transactions wrote the same key and this one lost the race. The client is expected to
    /// retry; the executor turns this into a `23505` when the key it lost was a unique index entry,
    /// because from the user's point of view that is a duplicate and not a race.
    #[error("could not serialize access due to concurrent update: {0}")]
    SerializationFailure(String),

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

    /// A bug here, not a mistake there. Nothing driven by user input may produce this.
    #[error("internal error: {0}")]
    Internal(String),
}

impl SqlError {
    /// The five-character SQLSTATE a client will branch on.
    #[must_use]
    pub fn sqlstate(&self) -> &'static str {
        match self {
            SqlError::FeatureNotSupported(_) => sqlstate::FEATURE_NOT_SUPPORTED,
            SqlError::Syntax { .. } => sqlstate::SYNTAX_ERROR,
            SqlError::StatementTooComplex => sqlstate::STATEMENT_TOO_COMPLEX,
            SqlError::UndefinedTable(_) => sqlstate::UNDEFINED_TABLE,
            SqlError::UndefinedColumn(_) => sqlstate::UNDEFINED_COLUMN,
            SqlError::DuplicateTable(_) => sqlstate::DUPLICATE_TABLE,
            SqlError::DuplicateColumn(_) => sqlstate::DUPLICATE_COLUMN,
            SqlError::UniqueViolation(_) => sqlstate::UNIQUE_VIOLATION,
            SqlError::NotNullViolation(_) => sqlstate::NOT_NULL_VIOLATION,
            SqlError::InvalidTextRepresentation { .. } => sqlstate::INVALID_TEXT_REPRESENTATION,
            SqlError::DatatypeMismatch(_) => sqlstate::DATATYPE_MISMATCH,
            SqlError::SerializationFailure(_) => sqlstate::SERIALIZATION_FAILURE,
            SqlError::InFailedTransaction => sqlstate::IN_FAILED_SQL_TRANSACTION,
            SqlError::ActiveTransaction => sqlstate::ACTIVE_SQL_TRANSACTION,
            SqlError::NoActiveTransaction => sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            SqlError::ConfigurationLimitExceeded(_) => sqlstate::CONFIGURATION_LIMIT_EXCEEDED,
            SqlError::ProtocolViolation(_) => sqlstate::PROTOCOL_VIOLATION,
            SqlError::InvalidSqlStatementName(_) => sqlstate::INVALID_SQL_STATEMENT_NAME,
            SqlError::InvalidCursorName(_) => sqlstate::INVALID_CURSOR_NAME,
            SqlError::InvalidPassword(_) => sqlstate::INVALID_PASSWORD,
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
            SqlError::ActiveTransaction | SqlError::NoActiveTransaction => Severity::Warning,
            SqlError::ProtocolViolation(_) | SqlError::InvalidPassword(_) => Severity::Fatal,
            _ => Severity::Error,
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
            SqlError::UniqueViolation("accounts_pkey".into()).to_string(),
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
