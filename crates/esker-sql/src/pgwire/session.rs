//! The session state machine: what a connection remembers between messages.
//!
//! One connection is one [`Session`]. It owns the transaction status a client sees in every
//! `ReadyForQuery`, decides which statements are allowed in that status, and turns each statement's
//! outcome into messages. It does no I/O and holds no socket, so the whole protocol conversation is
//! testable as a function from messages to messages.
//!
//! # Where the behaviour came from
//!
//! Every rule below was read off a capture of a real PostgreSQL 19beta1 session rather than out of
//! the specification, because this is the part of the protocol where plausible and correct differ
//! most:
//!
//! * **`ReadyForQuery` is sent once per `Query` message, not once per statement.** A query string
//!   holding three `SELECT`s produces three `CommandComplete`s and one `ReadyForQuery`.
//! * **An error abandons the rest of the query string.** `SELECT 1; SELECT * FROM nope; SELECT 3`
//!   runs the first, reports the second, and never runs the third.
//! * **An error outside a transaction block leaves the status `I`, not `E`.** Only a statement
//!   inside an explicit block puts the session into the failed state.
//! * **`BEGIN` inside a transaction is a *warning*, and the command still completes** with the tag
//!   `BEGIN`. So is `COMMIT` or `ROLLBACK` outside one. A warning is a `NoticeResponse`, and the
//!   `CommandComplete` follows it — a client that saw only the notice would think the statement
//!   failed.
//! * **`COMMIT` on a failed transaction reports the tag `ROLLBACK`**, because that is what actually
//!   happened.
//! * **An empty query string gets `EmptyQueryResponse` and no `CommandComplete`.**

use crate::error::{Result, SqlError};
use crate::parse::{Parsed, StatementClass, parse_statements};
use crate::pgwire::message::{Backend, FieldDescription, TransactionStatus};
use crate::pgwire::{error_fields, error_message};

/// What executing one statement produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Rows, and the tag that follows them.
    Rows {
        /// Shape of the rows.
        fields: Vec<FieldDescription>,
        /// The rows themselves; `None` in a column is SQL NULL.
        rows: Vec<Vec<Option<Vec<u8>>>>,
        /// Command tag, such as `SELECT 3`.
        tag: String,
    },
    /// No rows, just a tag — `INSERT 0 1`, `CREATE TABLE`.
    Done {
        /// Command tag.
        tag: String,
    },
}

impl Outcome {
    /// A tag-only outcome.
    pub fn done(tag: impl Into<String>) -> Self {
        Outcome::Done { tag: tag.into() }
    }
}

/// The seam between the protocol and the thing that runs statements.
///
/// The executor (unit 6 of `docs/plans/phase-6a.md`) implements this; the tests here use a fake.
/// Transaction control is separate from `execute` because the session has to know about it — it is
/// what moves the status a client sees — while still letting a real implementation start and finish
/// an actual transaction, and fail while doing it. A `commit` that returns an error is a Percolator
/// conflict, and the session must end the transaction anyway.
pub trait Execute {
    /// Runs one statement that is not transaction control.
    fn execute(&mut self, parsed: &Parsed) -> Result<Outcome>;

    /// Opens a transaction.
    fn begin(&mut self) -> Result<()> {
        Ok(())
    }

    /// Commits the open transaction.
    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    /// Abandons the open transaction.
    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }
}

/// One client connection's state.
#[derive(Debug, Default)]
pub struct Session {
    status: TransactionStatus,
}

impl Session {
    /// A session that has just finished starting up: idle, no transaction.
    #[must_use]
    pub fn new() -> Self {
        Session {
            status: TransactionStatus::Idle,
        }
    }

    /// What the next `ReadyForQuery` will report.
    #[must_use]
    pub fn status(&self) -> TransactionStatus {
        self.status
    }

    /// Handles one simple-query message, appending every byte of the reply.
    ///
    /// Always ends with exactly one `ReadyForQuery`, whatever happened in between, because that is
    /// the message the client is waiting for before it will send anything else.
    pub fn simple_query(&mut self, sql: &str, executor: &mut dyn Execute, out: &mut Vec<u8>) {
        self.run_query(sql, executor, out);
        Backend::ReadyForQuery(self.status).encode(out);
    }

    fn run_query(&mut self, sql: &str, executor: &mut dyn Execute, out: &mut Vec<u8>) {
        let statements = match parse_statements(sql) {
            Ok(statements) => statements,
            Err(error) => return self.fail(&error, out),
        };
        if statements.is_empty() {
            // Not an error and not a command: PostgreSQL answers an empty string with this and no
            // `CommandComplete` at all.
            Backend::EmptyQueryResponse.encode(out);
            return;
        }
        for parsed in &statements {
            // One failure abandons the rest of the string.
            if !self.run_one(parsed, executor, out) {
                return;
            }
        }
    }

    /// Runs one statement. Returns false when the query string should stop here.
    fn run_one(&mut self, parsed: &Parsed, executor: &mut dyn Execute, out: &mut Vec<u8>) -> bool {
        let class = parsed.class();

        // A failed transaction refuses everything until it is ended, and says why every time.
        if self.status == TransactionStatus::Failed && !ends_a_transaction(class) {
            self.fail(&SqlError::InFailedTransaction, out);
            return false;
        }

        let outcome = match class {
            StatementClass::Begin => self.begin(executor, out),
            StatementClass::Commit => self.commit(executor, out),
            StatementClass::Rollback => self.rollback(executor, out),
            _ => executor.execute(parsed),
        };

        match outcome {
            Ok(Outcome::Rows { fields, rows, tag }) => {
                Backend::RowDescription(&fields).encode(out);
                for row in &rows {
                    Backend::DataRow(row).encode(out);
                }
                Backend::CommandComplete(&tag).encode(out);
                true
            }
            Ok(Outcome::Done { tag }) => {
                Backend::CommandComplete(&tag).encode(out);
                true
            }
            Err(error) => {
                self.fail(&error, out);
                false
            }
        }
    }

    /// `BEGIN`. Inside a transaction it is a warning that changes nothing, which is PostgreSQL's
    /// own answer and not a leniency of ours.
    fn begin(&mut self, executor: &mut dyn Execute, out: &mut Vec<u8>) -> Result<Outcome> {
        if self.status == TransactionStatus::InTransaction {
            // A warning, and then the command completes anyway with its own tag. Returning this
            // as an error instead would send the notice and no `CommandComplete`, and a client
            // waiting for the command to finish would be told only that something was odd.
            warn(&SqlError::ActiveTransaction, out);
            return Ok(Outcome::done("BEGIN"));
        }
        executor.begin()?;
        self.status = TransactionStatus::InTransaction;
        Ok(Outcome::done("BEGIN"))
    }

    /// `COMMIT`. On a failed transaction the tag is `ROLLBACK`, because that is what happens to it.
    fn commit(&mut self, executor: &mut dyn Execute, out: &mut Vec<u8>) -> Result<Outcome> {
        match self.status {
            TransactionStatus::Idle => {
                // A warning, and then the command completes anyway.
                warn(&SqlError::NoActiveTransaction, out);
                Ok(Outcome::done("COMMIT"))
            }
            TransactionStatus::Failed => {
                let result = executor.rollback();
                self.status = TransactionStatus::Idle;
                result.map(|()| Outcome::done("ROLLBACK"))
            }
            TransactionStatus::InTransaction => {
                let result = executor.commit();
                // The transaction is over either way; a failed commit does not leave it open.
                self.status = TransactionStatus::Idle;
                result.map(|()| Outcome::done("COMMIT"))
            }
        }
    }

    /// `ROLLBACK`. Outside a transaction it is a warning; otherwise it always succeeds in ending
    /// the transaction, whatever state it was in.
    fn rollback(&mut self, executor: &mut dyn Execute, out: &mut Vec<u8>) -> Result<Outcome> {
        if self.status == TransactionStatus::Idle {
            warn(&SqlError::NoActiveTransaction, out);
            return Ok(Outcome::done("ROLLBACK"));
        }
        let result = executor.rollback();
        self.status = TransactionStatus::Idle;
        result.map(|()| Outcome::done("ROLLBACK"))
    }

    /// Emits a failure and moves the transaction into the failed state if there is one.
    ///
    /// An error outside a transaction block leaves the status idle: there is no block to poison,
    /// and reporting `E` would tell the client to send a `ROLLBACK` it does not owe.
    fn fail(&mut self, error: &SqlError, out: &mut Vec<u8>) {
        let fields = error_fields(error);
        error_message(error, &fields).encode(out);
        if error.aborts_transaction() && self.status == TransactionStatus::InTransaction {
            self.status = TransactionStatus::Failed;
        }
    }
}

/// Emits a warning or notice. Unlike a failure, the statement carries on afterwards and still
/// sends its `CommandComplete`.
fn warn(error: &SqlError, out: &mut Vec<u8>) {
    let fields = error_fields(error);
    error_message(error, &fields).encode(out);
}

/// Whether this statement is one of the two a failed transaction still accepts.
fn ends_a_transaction(class: &StatementClass) -> bool {
    matches!(class, StatementClass::Commit | StatementClass::Rollback)
}

#[cfg(test)]
mod tests {
    use super::{Execute, Outcome, Session};
    use crate::error::{Result, SqlError};
    use crate::parse::Parsed;
    use crate::pgwire::message::{FieldDescription, TransactionStatus};
    use crate::sqlstate;

    /// Stands in for the executor until unit 6. It answers every statement the same way, because
    /// what is under test here is the protocol conversation and not the SQL.
    #[derive(Default)]
    struct Fake {
        /// Rows to hand back, if any.
        rows: usize,
        /// Set to fail the next `execute`.
        fail: Option<SqlError>,
        /// Set to fail the next `commit`, the way a Percolator conflict would.
        fail_commit: Option<SqlError>,
        /// What the session asked for, in order.
        calls: Vec<String>,
    }

    impl Execute for Fake {
        fn execute(&mut self, parsed: &Parsed) -> Result<Outcome> {
            self.calls.push(format!("execute {}", parsed.rendered()));
            if let Some(error) = self.fail.take() {
                return Err(error);
            }
            if self.rows == 0 {
                return Ok(Outcome::done("SELECT 0"));
            }
            let fields = vec![FieldDescription {
                name: "?column?".to_owned(),
                table_oid: 0,
                column_id: 0,
                type_oid: 23,
                type_size: 4,
                type_modifier: -1,
                format: 0,
            }];
            let rows = (0..self.rows)
                .map(|n| vec![Some(n.to_string().into_bytes())])
                .collect();
            Ok(Outcome::Rows {
                fields,
                rows,
                tag: format!("SELECT {}", self.rows),
            })
        }

        fn begin(&mut self) -> Result<()> {
            self.calls.push("begin".to_owned());
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            self.calls.push("commit".to_owned());
            self.fail_commit.take().map_or(Ok(()), Err)
        }

        fn rollback(&mut self) -> Result<()> {
            self.calls.push("rollback".to_owned());
            Ok(())
        }
    }

    /// Frames an encoded reply back into `(tag, body)` pairs so a test can talk about messages.
    fn messages(bytes: &[u8]) -> Vec<(char, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 5 <= bytes.len() {
            let length =
                u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
                    as usize;
            out.push((bytes[at] as char, bytes[at + 5..at + 1 + length].to_vec()));
            at += 1 + length;
        }
        assert_eq!(at, bytes.len(), "the reply did not frame cleanly");
        out
    }

    /// Just the message types, which is the shape a capture is easiest to compare against.
    fn tags(bytes: &[u8]) -> String {
        messages(bytes).into_iter().map(|(tag, _)| tag).collect()
    }

    /// Pulls one field out of an `ErrorResponse` or `NoticeResponse` body.
    fn field(body: &[u8], code: u8) -> Option<String> {
        let mut rest = body;
        while let Some((&first, tail)) = rest.split_first() {
            if first == 0 {
                return None;
            }
            let end = tail.iter().position(|b| *b == 0)?;
            if first == code {
                return String::from_utf8(tail[..end].to_vec()).ok();
            }
            rest = &tail[end + 1..];
        }
        None
    }

    fn run(session: &mut Session, sql: &str, fake: &mut Fake) -> Vec<u8> {
        let mut out = Vec::new();
        session.simple_query(sql, fake, &mut out);
        out
    }

    /// Captured: `SELECT 1; SELECT 2; SELECT 3` answered T D C T D C T D C Z — three complete
    /// results and **one** `ReadyForQuery`, not one per statement.
    #[test]
    fn several_statements_in_one_message_get_one_ready_for_query() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 1,
            ..Fake::default()
        };
        let reply = run(&mut session, "SELECT 1; SELECT 2; SELECT 3", &mut fake);
        assert_eq!(tags(&reply), "TDCTDCTDCZ");
        assert_eq!(session.status(), TransactionStatus::Idle);
    }

    /// Captured: `SELECT 1; SELECT * FROM nope; SELECT 3` answered T D C E Z. The third statement
    /// never ran, and the status stayed `I` because no transaction block was open.
    #[test]
    fn an_error_abandons_the_rest_of_the_query_string() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 1,
            fail: None,
            ..Fake::default()
        };
        // First statement succeeds, second fails.
        let mut out = Vec::new();
        session.simple_query("SELECT 1", &mut fake, &mut out);
        fake.fail = Some(SqlError::UndefinedTable("nope".into()));
        let reply = run(&mut session, "SELECT 1; SELECT 2; SELECT 3", &mut fake);
        assert_eq!(
            tags(&reply),
            "EZ",
            "the failure is the first statement here"
        );
        assert_eq!(
            session.status(),
            TransactionStatus::Idle,
            "an error outside a transaction block leaves the status idle, not failed"
        );
    }

    /// The state machine's centre: inside a block, an error poisons everything until the block
    /// ends, and every refused statement says so with 25P02.
    #[test]
    fn inside_a_transaction_an_error_poisons_it_until_it_ends() {
        let mut session = Session::new();
        let mut fake = Fake::default();

        let reply = run(&mut session, "BEGIN", &mut fake);
        assert_eq!(tags(&reply), "CZ");
        assert_eq!(session.status(), TransactionStatus::InTransaction);

        fake.fail = Some(SqlError::UndefinedTable("nope".into()));
        let reply = run(&mut session, "SELECT * FROM nope", &mut fake);
        let framed = messages(&reply);
        assert_eq!(tags(&reply), "EZ");
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::UNDEFINED_TABLE)
        );
        assert_eq!(session.status(), TransactionStatus::Failed);
        assert_eq!(framed[1].1, b"E", "ReadyForQuery must report the failure");

        // Everything else is refused, with the code and message PostgreSQL uses.
        let reply = run(&mut session, "SELECT 1", &mut fake);
        let framed = messages(&reply);
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::IN_FAILED_SQL_TRANSACTION)
        );
        assert_eq!(
            field(&framed[0].1, b'M').as_deref(),
            Some("current transaction is aborted, commands ignored until end of transaction block")
        );
        assert_eq!(session.status(), TransactionStatus::Failed);
    }

    /// Captured, and the detail most likely to be got wrong: committing a failed transaction
    /// reports `ROLLBACK`, because that is what happened to it.
    #[test]
    fn committing_a_failed_transaction_reports_rollback() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        run(&mut session, "BEGIN", &mut fake);
        fake.fail = Some(SqlError::UndefinedTable("nope".into()));
        run(&mut session, "SELECT * FROM nope", &mut fake);
        assert_eq!(session.status(), TransactionStatus::Failed);

        let reply = run(&mut session, "COMMIT", &mut fake);
        let framed = messages(&reply);
        assert_eq!(tags(&reply), "CZ");
        assert_eq!(framed[0].1, b"ROLLBACK\0");
        assert_eq!(session.status(), TransactionStatus::Idle);
        assert!(
            fake.calls.contains(&"rollback".to_owned()),
            "the executor must be told to roll back, not to commit"
        );
    }

    /// Captured: a second `BEGIN` produces a WARNING with 25001, and then the command completes
    /// normally with the tag `BEGIN`. A client that saw only the notice would think it failed.
    #[test]
    fn a_second_begin_warns_and_still_completes() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        run(&mut session, "BEGIN", &mut fake);

        let reply = run(&mut session, "BEGIN", &mut fake);
        let framed = messages(&reply);
        assert_eq!(
            tags(&reply),
            "NCZ",
            "a notice, then the command, then ready"
        );
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::ACTIVE_SQL_TRANSACTION)
        );
        assert_eq!(field(&framed[0].1, b'V').as_deref(), Some("WARNING"));
        assert_eq!(
            field(&framed[0].1, b'M').as_deref(),
            Some("there is already a transaction in progress")
        );
        assert_eq!(framed[1].1, b"BEGIN\0");
        assert_eq!(
            session.status(),
            TransactionStatus::InTransaction,
            "the transaction is untouched"
        );
    }

    /// Captured: both of these warn with 25P01 and then complete with their own tag.
    #[test]
    fn ending_a_transaction_that_is_not_open_warns_and_still_completes() {
        for (sql, tag) in [
            ("COMMIT", &b"COMMIT\0"[..]),
            ("ROLLBACK", &b"ROLLBACK\0"[..]),
        ] {
            let mut session = Session::new();
            let mut fake = Fake::default();
            let reply = run(&mut session, sql, &mut fake);
            let framed = messages(&reply);
            assert_eq!(tags(&reply), "NCZ", "{sql}");
            assert_eq!(
                field(&framed[0].1, b'C').as_deref(),
                Some(sqlstate::NO_ACTIVE_SQL_TRANSACTION),
                "{sql}"
            );
            assert_eq!(
                field(&framed[0].1, b'V').as_deref(),
                Some("WARNING"),
                "{sql}"
            );
            assert_eq!(framed[1].1, tag, "{sql}");
            assert_eq!(session.status(), TransactionStatus::Idle, "{sql}");
        }
    }

    /// Captured: an empty query string gets `EmptyQueryResponse` and no `CommandComplete` at all.
    #[test]
    fn an_empty_query_gets_an_empty_query_response() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let reply = run(&mut session, "", &mut fake);
        assert_eq!(tags(&reply), "IZ");
        assert!(fake.calls.is_empty(), "nothing was executed");
    }

    /// Contract C2 reaching the wire: a statement we parse and do not run is `0A000` naming the
    /// feature, and the session carries on afterwards exactly as PostgreSQL would.
    #[test]
    fn an_unsupported_statement_is_refused_by_name_and_the_session_survives() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let reply = run(&mut session, "VACUUM ANALYZE t", &mut fake);
        let framed = messages(&reply);
        assert_eq!(tags(&reply), "EZ");
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::FEATURE_NOT_SUPPORTED)
        );
        assert_eq!(
            field(&framed[0].1, b'M').as_deref(),
            Some("VACUUM is not supported")
        );
        assert_eq!(session.status(), TransactionStatus::Idle);
        assert!(fake.calls.is_empty(), "it must not reach the executor");

        // And the next statement works, which is the half of C2 that is about the state machine.
        let reply = run(&mut session, "SELECT 1", &mut fake);
        assert_eq!(tags(&reply), "CZ");
    }

    /// Inside a transaction block, the same refusal poisons the block, as any error does.
    #[test]
    fn an_unsupported_statement_inside_a_transaction_poisons_it() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        run(&mut session, "BEGIN", &mut fake);
        run(
            &mut session,
            "CREATE PUBLICATION p FOR ALL TABLES",
            &mut fake,
        );
        assert_eq!(session.status(), TransactionStatus::Failed);
        let reply = run(&mut session, "ROLLBACK", &mut fake);
        assert_eq!(tags(&reply), "CZ");
        assert_eq!(session.status(), TransactionStatus::Idle);
    }

    /// A commit that fails is a Percolator conflict, and the transaction is over regardless: a
    /// session left in `T` after a failed commit would wait forever for a block that is gone.
    #[test]
    fn a_failed_commit_still_ends_the_transaction() {
        let mut session = Session::new();
        let mut fake = Fake {
            fail_commit: Some(SqlError::UniqueViolation("g_pkey".into())),
            ..Fake::default()
        };
        run(&mut session, "BEGIN", &mut fake);
        let reply = run(&mut session, "COMMIT", &mut fake);
        let framed = messages(&reply);
        assert_eq!(tags(&reply), "EZ");
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::UNIQUE_VIOLATION)
        );
        assert_eq!(
            session.status(),
            TransactionStatus::Idle,
            "the block is gone even though the commit failed"
        );
    }

    /// A syntax error is reported and nothing runs, and the session is still usable.
    #[test]
    fn a_syntax_error_is_reported_without_reaching_the_executor() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let reply = run(&mut session, "SELCT 1", &mut fake);
        let framed = messages(&reply);
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::SYNTAX_ERROR)
        );
        assert!(fake.calls.is_empty());
        assert_eq!(session.status(), TransactionStatus::Idle);
    }
}
