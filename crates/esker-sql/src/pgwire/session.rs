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

use std::collections::BTreeMap;

use crate::error::{Result, SqlError};
use crate::parse::{Parsed, StatementClass, parse_statements};
use crate::pgwire::message::{Backend, FieldDescription, Frontend, Target, TransactionStatus};
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

/// The parameters a `Bind` carried, and everything needed to read them.
///
/// Values arrive as bytes with a format code each, and the *type* to read them as is not in the
/// message at all — it is inferred from where the parameter appears in the statement, which is why
/// this is handed to the executor rather than decoded here.
#[derive(Debug, Clone, Copy)]
pub struct Params<'a> {
    /// One per parameter; `None` is SQL NULL, which the wire spells as a length of -1.
    pub values: &'a [Option<Vec<u8>>],
    /// Format codes: one per value, or exactly one meaning "all of them", or none meaning all
    /// text. That three-way rule is the protocol's, and getting it wrong reads a binary value as
    /// text or the reverse.
    pub formats: &'a [i16],
    /// Type OIDs the client declared in `Parse`. May be shorter than `values`, and a zero means
    /// "you decide".
    pub declared: &'a [u32],
    /// Whether a `Bind` produced these — **which changes the error for a missing value**.
    ///
    /// The simple query protocol never binds, and a `$1` in it is `42P02 there is no parameter $1`.
    /// The extended one always binds, and a count that does not match is a *protocol* error,
    /// `08P01 bind message supplies 0 parameters, but prepared statement "" requires 1`. The two
    /// are indistinguishable from the values alone — a `Bind` carrying none looks like no `Bind` —
    /// so the path says which it was.
    pub bound: bool,
}

impl Params<'_> {
    /// No parameters at all — what the simple query protocol always has.
    pub const NONE: Params<'static> = Params {
        values: &[],
        formats: &[],
        declared: &[],
        bound: false,
    };

    /// The format code for one parameter, with the protocol's three-way rule applied.
    #[must_use]
    pub fn format(&self, index: usize) -> i16 {
        match self.formats {
            [] => 0,
            [only] => *only,
            many => many.get(index).copied().unwrap_or(0),
        }
    }
}

/// What `Describe` on a prepared statement answers: the parameters it takes and the rows it
/// returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Described {
    /// One type OID per parameter, inferred where the client declared nothing.
    pub parameters: Vec<u32>,
    /// The row shape, or `None` for a statement that returns none — which the protocol spells
    /// `NoData`.
    pub fields: Option<Vec<FieldDescription>>,
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
    fn execute(&mut self, parsed: &Parsed, params: &Params<'_>) -> Result<Outcome>;

    /// Whether another session has called `pg_terminate_backend` on this one.
    ///
    /// **Required rather than defaulted**, unlike the optional capabilities below it. A default of
    /// `false` reads as "this executor cannot be terminated" and would be indistinguishable from an
    /// implementor that forgot to override it — and the failure that produces is a session the
    /// server believes is alive and a client that has been told it is dead. An executor with no
    /// session registry behind it answers `false` in one line and says so.
    fn terminated(&self) -> bool;

    /// Hands the executor this session's prepared statements, for `pg_prepared_statements`.
    ///
    /// Called immediately before every statement runs, and passing the whole list each time is
    /// deliberate: the store lives in [`Session`] and this is a *copy* of it, so the only way the
    /// two cannot disagree is for the copy to be rebuilt at the moment it can be read. An
    /// executor that keeps its own tally, updated as statements come and go, is the arrangement
    /// that drifts.
    ///
    /// **Required rather than defaulted**, for `terminated`'s reason and one of its own: an
    /// implementor that silently did nothing would answer every read of the view with no rows,
    /// which is indistinguishable from a session that has prepared nothing. An executor with no
    /// catalog behind it discards the list in one line and says so.
    fn remember_prepared(&mut self, statements: Vec<crate::session::PreparedStatement>);

    /// Applies one parameter the startup packet's `options` asked for.
    ///
    /// **Not a `SET` statement built out of the client's text.** The name and the value come off
    /// the wire, and turning them back into SQL to parse would be building a statement out of
    /// something a client wrote — for a setting the same client could set with a `SET` a moment
    /// later, so there is nothing to gain and a shape to get wrong.
    ///
    /// **Required rather than defaulted**: a default that did nothing would leave a client
    /// believing a setting it does not have, which is the one outcome the connection refusal
    /// exists to prevent.
    ///
    /// # Errors
    ///
    /// Whatever the parameter itself refuses — an unrecognised name, or a value the parameter does
    /// not take. Both end the connection before it opens, which is what a real server does.
    fn set_option(&mut self, name: &str, value: &str) -> Result<()>;

    /// Explains `statement` under the options `explain` was written with.
    ///
    /// **Two halves, because they live in two places.** `EXPLAIN … EXECUTE p1(1)` explains a
    /// statement the *session* holds and the executor has never seen, so the session resolves the
    /// name and hands both trees over: the options are read off `explain` and the statement is
    /// lowered from `statement`. Nothing new crosses this boundary — both are already the currency
    /// of `execute` and `describe`.
    ///
    /// **Required rather than defaulted**, for `terminated`'s reason: a default would answer an
    /// empty plan for a statement that has one, and an empty plan looks like an answer.
    fn explain_prepared(
        &mut self,
        explain: &Parsed,
        statement: &Parsed,
        params: &Params<'_>,
    ) -> Result<Outcome>;

    /// What a statement takes and what it returns, without running it — what `Describe` needs.
    ///
    /// The default answers "no parameters, no rows", which is right for an executor that runs
    /// nothing.
    fn describe(&mut self, parsed: &Parsed, declared: &[u32]) -> Result<Described> {
        let _ = (parsed, declared);
        Ok(Described::default())
    }

    /// Notices the statement that just ran produced, which the session sends before its
    /// `CommandComplete`.
    ///
    /// A notice is not an outcome: `CREATE TABLE IF NOT EXISTS` for a table that is already there
    /// *succeeds*, and PostgreSQL says `relation "t" already exists, skipping` on the way. Draining
    /// them after the call keeps them out of [`Outcome`], which is about what a statement produced
    /// rather than what it remarked on.
    fn take_notices(&mut self) -> Vec<SqlError> {
        Vec::new()
    }

    /// Marks a point in the open block that `rollback_to` can return to.
    ///
    /// Names **stack**: two savepoints of one name are two marks, and `rollback_to` and `release`
    /// each find the most recent. A map from name to mark gets that wrong in a way no
    /// single-savepoint test can see (measured; `tests/corpus/pg19_savepoint.txt`).
    fn savepoint(&mut self, name: &str) -> Result<()> {
        let _ = name;
        Ok(())
    }

    /// Undoes every write made since the most recent mark of that name, and leaves the mark.
    fn rollback_to(&mut self, name: &str) -> Result<()> {
        let _ = name;
        Ok(())
    }

    /// Drops the most recent mark of that name and every mark above it, keeping their writes.
    fn release(&mut self, name: &str) -> Result<()> {
        let _ = name;
        Ok(())
    }

    /// Opens an explicit transaction block. `read_only` is `BEGIN READ ONLY`, which refuses every
    /// write in the block with `25006` exactly as PostgreSQL does.
    fn begin(&mut self, read_only: bool) -> Result<()> {
        let _ = read_only;
        Ok(())
    }

    /// The isolation level this transaction runs at, from a `BEGIN ISOLATION LEVEL …`.
    ///
    /// Called **after** `begin`, because the block's parameters are saved there and a level named
    /// on the `BEGIN` belongs inside the block it starts — so it is restored when the block ends,
    /// which is what a real server does (ADR 0057).
    fn set_isolation(&mut self, level: crate::parameter::Isolation) -> Result<()> {
        let _ = level;
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

    /// **The session is ending: give back everything that outlives a statement.**
    ///
    /// Advisory locks, the open transaction, and whatever a session owns that a statement does
    /// not — [`crate::exec::Executor`] also takes its temporary schema back here
    /// ([ADR 0054](../../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md)).
    ///
    /// **Required rather than defaulted**, for the reason [`Execute::terminated`] is: a default
    /// that does nothing reads as "this executor holds nothing a session outlives" and is
    /// indistinguishable from an implementor that forgot — and what that produces is a lock or a
    /// schema nobody can reach and nothing that says so. Three of the four implementors in this
    /// repository really do hold nothing, and each says so in a line.
    ///
    /// **Called from exactly one place** — the single ending in
    /// [`crate::pgwire::server::Connection::run`] — and **on the blocking pool**, because for a
    /// real executor this reaches the stores: a rollback is a Percolator rollback and dropping a
    /// schema is a transaction of its own. That is the whole of `#83`. It used to be five
    /// `release_advisory_locks` calls at five of the seven ways out of that loop, with the
    /// transaction and the schema left to `Executor`'s destructor — which ran on a runtime thread,
    /// where [`esker_proto::transport::BlockingTransport`] refuses rather than blocking,
    /// so the cleanup was skipped in silence.
    fn close(&mut self);

    /// Releases every advisory lock this session holds, because it is ending.
    ///
    /// **Not tidying: the other half of the lifetime.** An advisory lock survives `ROLLBACK` and
    /// survives the statement that took it, so the only two things that release one are an
    /// explicit unlock and the session going away — and the connection is the only thing that
    /// knows about the second (`crate::advisory`).
    ///
    /// Reached through [`Execute::close`], which is the one ending; this used to say "every path
    /// out of `Connection::run` calls it" and that was false for two of the seven — a message that
    /// would not decode, and every `?`.
    ///
    /// Does nothing by default, which is right for an executor that has no locks.
    fn release_advisory_locks(&self) {}

    /// How long this session may sit **idle inside a transaction block** before the server ends
    /// the connection, or `None` for no limit.
    ///
    /// It is on this trait rather than read from a parameter table by the connection, because the
    /// value is a property of the *session* — a `SET` changes it mid-connection — and the executor
    /// is what holds a session's settings. The connection is what owns the socket and therefore
    /// the only place the wait can be bounded (`crate::pgwire::server::Connection::run`).
    ///
    /// `None` by default, which is right for an executor with no settings at all.
    fn idle_in_transaction_timeout(&self) -> Option<std::time::Duration> {
        None
    }
}

/// A prepared statement resolved by name, with its arguments bound.
///
/// What `EXECUTE` and `EXPLAIN … EXECUTE` both need: they disagree about what to do with the
/// statement and about nothing else.
struct Bound {
    /// The statement itself, out of the session's store.
    statement: Parsed,
    /// Its arguments, as a `Bind`'s values.
    values: Vec<Option<Vec<u8>>>,
    /// The parameter types the client declared, if any.
    declared: Vec<u32>,
    /// What `Describe` last answered for it, which is the cached plan's baseline.
    baseline: Option<Described>,
}

/// A statement that has been parsed and named, waiting to be bound.
///
/// `parsed` is `None` for the empty statement — `Parse` with an empty query string is legal, binds
/// and describes normally, and yields `EmptyQueryResponse` when executed. A real capture confirms
/// each of those, and treating it as an error instead would break any driver that probes with one.
#[derive(Debug, Clone)]
struct Prepared {
    parsed: Option<Parsed>,
    /// Parameter type OIDs as the client declared them in `Parse`.
    param_types: Vec<u32>,
    /// The text `pg_prepared_statements.statement` reports, verbatim as the client sent it.
    source: String,
    /// Whether SQL `PREPARE` made it, as against the protocol's `Parse`.
    from_sql: bool,
    /// What analysis last answered for it: the parameter types it resolved and the row shape.
    ///
    /// **Filled when the statement is prepared or described, never when it is merely parsed.** A
    /// SQL `PREPARE` is analysed at once — a missing relation is `42P01` there, measured — so its
    /// types are known before it can be run. A `Parse` is not analysed until something asks, so a
    /// wire statement nobody has described reports the OIDs the client declared and no result
    /// types at all. Analysing every `Parse` to fill a view would be a change to when errors are
    /// raised, which is a behaviour change and not a view.
    described: Option<Described>,
}

/// A prepared statement with its parameters bound, ready to run.
#[derive(Debug, Clone)]
struct Portal {
    statement: String,
    /// Kept so a second `Execute` after a `PortalSuspended` can resume, and so `Describe` can
    /// answer without the statement being re-bound.
    params: Vec<Option<Vec<u8>>>,
    /// Format codes as `Bind` sent them, with the protocol's "one for all" rule left intact —
    /// [`Params::format`] applies it.
    formats: Vec<i16>,
    /// Rows already returned by earlier `Execute`s against this portal.
    delivered: usize,
}

/// One client connection's state.
#[derive(Debug, Default)]
pub struct Session {
    status: TransactionStatus,
    statements: BTreeMap<String, Prepared>,
    portals: BTreeMap<String, Portal>,
    /// Set by any failure in the extended protocol. While it is set, every message but `Sync` is
    /// discarded in silence — not answered, not refused. `Sync` clears it and is the only thing
    /// that sends `ReadyForQuery`.
    skipping_until_sync: bool,
}

impl Session {
    /// A session that has just finished starting up: idle, no transaction.
    #[must_use]
    pub fn new() -> Self {
        Session::default()
    }

    /// What the next `ReadyForQuery` will report.
    #[must_use]
    pub fn status(&self) -> TransactionStatus {
        self.status
    }

    /// `PREPARE name [(types)] AS stmt`, into the store the protocol's `Parse` writes to.
    ///
    /// **One store, two doors.** `DISCARD ALL` already clears it, and a client that prepares `p`
    /// over SQL is naming the same thing a `Bind` on the protocol name `p` would — which is what
    /// PostgreSQL does and the reason this is not a second map beside the first.
    ///
    /// The body arrives as **text** from [`crate::parse::StatementClass::Prepare`], because
    /// `sqlparser` is named nowhere outside the parse module. Re-parsing it here is not a detour:
    /// it is what lets a `PREPARE` this node cannot run refuse at the `PREPARE`, as a real server
    /// does.
    fn prepare_sql(
        &mut self,
        name: String,
        body: &str,
        declared: &[u32],
        source: &str,
        executor: &mut dyn Execute,
    ) -> Result<Outcome> {
        if self.statements.contains_key(&name) {
            return Err(SqlError::DuplicatePreparedStatement(name));
        }
        let mut prepared = parse_statements(body)?;
        if prepared.len() != 1 {
            return Err(SqlError::unsupported("PREPARE of more than one statement"));
        }
        let body = prepared.remove(0);
        // **Analysed here, not at the first `EXECUTE`.** Measured on 19beta1: `PREPARE x AS SELECT
        // * FROM nope` is `42P01` and a missing column is `42703`, both at the `PREPARE`. A node
        // that stored the text and failed later would report the mistake against a statement the
        // user is no longer looking at. `describe` resolves without running.
        //
        // Its answer is **kept**, and twice over: it is what `pg_prepared_statements` reports for
        // this statement, and it is the row shape every later `EXECUTE` is compared against
        // ([`revalidate`]).
        // **With the types the statement declared**, which is what `PREPARE p (int) AS SELECT $1`
        // is for: a parameter typed only by its declaration is `integer` on a real server, and
        // `pg_prepared_statements.parameter_types` says so. Measured, `{integer}` and
        // `{bigint,text}`. An undeclared one is inferred from its context, as before.
        let described = executor.describe(&body, declared)?;
        self.statements.insert(
            name,
            Prepared {
                parsed: Some(body),
                param_types: declared.to_vec(),
                source: source.to_owned(),
                from_sql: true,
                described: Some(described),
            },
        );
        Ok(Outcome::done("PREPARE"))
    }

    /// `EXECUTE name [(args)]`, running the stored statement with its arguments bound.
    ///
    /// **The arguments go in as `Params`, the same way a `Bind`'s do**, so everything the extended
    /// protocol already does for a `$1` — inferring its type from the column beside it, and the
    /// assignment cast into that column — happens here without a second implementation.
    /// `EXPLAIN [(options)] EXECUTE name [(args)]` — the options here, the statement in the store.
    ///
    /// **The same resolution `EXECUTE` does**, which is why it is the same function: the name is
    /// looked up in the one store both doors write to, the argument count is checked against the
    /// statement, and every refusal is the one a bare `EXECUTE` would have given — `26000` for a
    /// name that is not there and `42601` for the wrong number of arguments, both measured on the
    /// oracle through `EXPLAIN` itself.
    ///
    /// `connection_test.rb`'s `test_statement_key_is_logged` is why this exists: it names a
    /// statement with `PQprepare` and then asks `EXPLAIN (FORMAT JSON) EXECUTE <that name>(1)`,
    /// so the two doors have to meet here as they already do at `EXECUTE`.
    fn explain_execute(
        &mut self,
        name: String,
        args: Option<Vec<Option<String>>>,
        explain: &Parsed,
        executor: &mut dyn Execute,
    ) -> Result<Outcome> {
        let bound = self.bound_statement(name, args)?;
        // **The same revalidation an `EXECUTE` does.** Measured: a statement whose result type
        // changed is `0A000` from `EXPLAIN … EXECUTE` too, because the plan being explained is the
        // cached one and revalidating it is what explaining it means.
        revalidate(
            &bound.statement,
            bound.baseline.as_ref(),
            &bound.declared,
            executor,
        )?;
        executor.explain_prepared(
            explain,
            &bound.statement,
            &Params {
                values: &bound.values,
                formats: &[],
                declared: &bound.declared,
                bound: true,
            },
        )
    }

    /// The stored statement a name refers to, and its arguments as a `Bind`'s values.
    ///
    /// One reader for `EXECUTE` and `EXPLAIN … EXECUTE`: they disagree about what to do with the
    /// statement and about nothing else, and two lookups would be two chances to fold a quoted
    /// name differently or to check the argument count against the wrong thing.
    fn bound_statement(&self, name: String, args: Option<Vec<Option<String>>>) -> Result<Bound> {
        let Some(args) = args else {
            return Err(SqlError::unsupported(
                "an EXECUTE argument that is not a literal",
            ));
        };
        let Some(stored) = self.statements.get(&name) else {
            return Err(SqlError::InvalidSqlStatementName(name));
        };
        let Some(body) = stored.parsed.as_ref() else {
            return Err(SqlError::InvalidSqlStatementName(name));
        };
        let wanted = crate::exec::bind::parameter_count(&body.lower()?);
        if args.len() != wanted {
            return Err(SqlError::WrongParameterCount {
                name,
                expected: wanted,
                got: args.len(),
            });
        }
        Ok(Bound {
            // Cloned out of the store because the caller hands it to `&mut dyn Execute` and the
            // borrow of `self.statements` would otherwise outlive it.
            statement: body.clone(),
            values: args
                .into_iter()
                .map(|arg| arg.map(String::into_bytes))
                .collect(),
            // The declared types reach the revalidation and the execution both: without them a
            // `PREPARE p (int) AS SELECT $1` was re-described as `text` and refused against its
            // own baseline as a changed result type.
            declared: stored.param_types.clone(),
            baseline: stored.described.clone(),
        })
    }

    fn execute_sql(
        &mut self,
        name: String,
        args: Option<Vec<Option<String>>>,
        executor: &mut dyn Execute,
    ) -> Result<Outcome> {
        let bound = self.bound_statement(name, args)?;
        revalidate(
            &bound.statement,
            bound.baseline.as_ref(),
            &bound.declared,
            executor,
        )?;
        executor.execute(
            &bound.statement,
            &Params {
                values: &bound.values,
                formats: &[],
                declared: &bound.declared,
                bound: true,
            },
        )
    }

    /// `DEALLOCATE name` and `DEALLOCATE ALL`.
    ///
    /// A name that is not there is `26000`, the same refusal `EXECUTE` gives — measured; `ALL` over
    /// an empty store is not an error and answers `DEALLOCATE ALL`, its own command tag.
    fn deallocate_sql(&mut self, name: Option<String>) -> Result<Outcome> {
        match name {
            None => {
                self.statements.clear();
                Ok(Outcome::done("DEALLOCATE ALL"))
            }
            Some(name) => match self.statements.remove(&name) {
                Some(_) => Ok(Outcome::done("DEALLOCATE")),
                None => Err(SqlError::InvalidSqlStatementName(name)),
            },
        }
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
        if self.status == TransactionStatus::Failed && !allowed_in_a_failed_transaction(class) {
            self.fail(&SqlError::InFailedTransaction, out);
            return false;
        }

        let outcome = match class {
            StatementClass::Begin => self.begin(parsed, executor, out),
            StatementClass::Commit => self.commit(executor, out),
            StatementClass::Rollback => self.rollback(executor, out),
            StatementClass::Savepoint(name) => self.savepoint(name, executor),
            StatementClass::RollbackTo(name) => self.rollback_to(name, executor),
            StatementClass::Release(name) => self.release(name, executor),
            // A simple query carries no parameters: the protocol has no way to send one, which
            // is why `$1` in a `Query` is `42P02`.
            _ => self.run_statement(parsed, executor),
        };

        // Notices come before whatever the statement produced, error or not: a `CREATE TABLE IF
        // NOT EXISTS` that skipped succeeded *and* had something to say, and a client that saw the
        // `CommandComplete` first would attribute the notice to the next statement.
        for notice in executor.take_notices() {
            warn(&notice, out);
        }

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
    fn begin(
        &mut self,
        parsed: &Parsed,
        executor: &mut dyn Execute,
        out: &mut Vec<u8>,
    ) -> Result<Outcome> {
        if self.status == TransactionStatus::InTransaction {
            // A warning, and then the command completes anyway with its own tag. Returning this
            // as an error instead would send the notice and no `CommandComplete`, and a client
            // waiting for the command to finish would be told only that something was odd.
            warn(&SqlError::ActiveTransaction, out);
            return Ok(Outcome::done("BEGIN"));
        }
        executor.begin(parsed.begins_read_only())?;
        if let Some(level) = parsed.begins_isolation() {
            executor.set_isolation(level)?;
        }
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

    /// `SAVEPOINT <name>`: a mark in the open block, and `25P01` outside one.
    ///
    /// PostgreSQL names **its own verb** in that message rather than what the user typed, so all
    /// three of these say `SAVEPOINT`, `ROLLBACK TO SAVEPOINT` and `RELEASE SAVEPOINT` whichever
    /// spelling arrived. Measured.
    fn savepoint(&mut self, name: &str, executor: &mut dyn Execute) -> Result<Outcome> {
        if self.status != TransactionStatus::InTransaction {
            return Err(SqlError::OutsideTransactionBlock("SAVEPOINT"));
        }
        executor
            .savepoint(name)
            .map(|()| Outcome::done("SAVEPOINT"))
    }

    /// `ROLLBACK TO [SAVEPOINT] <name>`: undoes back to the mark and **leaves it there**, so the
    /// same savepoint can be rolled back to again.
    ///
    /// It is the one statement that recovers an aborted block, so a success here moves the status
    /// back to `InTransaction`. A failure does not: `3B001` for a savepoint that is not there
    /// aborts the block like any other error, which is one of the ways *into* `25P02`.
    fn rollback_to(&mut self, name: &str, executor: &mut dyn Execute) -> Result<Outcome> {
        if self.status == TransactionStatus::Idle {
            return Err(SqlError::OutsideTransactionBlock("ROLLBACK TO SAVEPOINT"));
        }
        executor.rollback_to(name)?;
        self.status = TransactionStatus::InTransaction;
        Ok(Outcome::done("ROLLBACK"))
    }

    /// `RELEASE [SAVEPOINT] <name>`: drops the mark and everything above it, and touches no data.
    fn release(&mut self, name: &str, executor: &mut dyn Execute) -> Result<Outcome> {
        if self.status != TransactionStatus::InTransaction {
            return Err(SqlError::OutsideTransactionBlock("RELEASE SAVEPOINT"));
        }
        executor.release(name).map(|()| Outcome::done("RELEASE"))
    }

    /// Handles one frontend message, appending whatever it should answer with.
    ///
    /// The one entry point a connection needs: `Query` runs the simple protocol and answers with
    /// its own `ReadyForQuery`, while the extended-protocol messages answer piecemeal and only
    /// `Sync` reports readiness.
    pub fn handle(&mut self, message: &Frontend, executor: &mut dyn Execute, out: &mut Vec<u8>) {
        // After a failure, everything up to the next `Sync` is discarded without a word. Captured
        // from a real server: a `Bind` and an `Execute` sent after a failed `Parse` produced no
        // bytes at all, and only `Sync` answered. A server that replied to them instead would put
        // one extra message in the stream and desynchronise the client for the rest of the session.
        if self.skipping_until_sync && !matches!(message, Frontend::Sync | Frontend::Terminate) {
            return;
        }
        match message {
            Frontend::Query(sql) => self.simple_query(sql, executor, out),
            Frontend::Parse {
                statement,
                sql,
                param_types,
            } => self.parse_message(statement, sql, param_types, out),
            Frontend::Bind {
                portal,
                statement,
                params,
                param_formats,
                ..
            } => self.bind(portal, statement, params, param_formats, out),
            Frontend::Describe { target, name } => self.describe(*target, name, executor, out),
            Frontend::Execute { portal, max_rows } => {
                self.execute(portal, *max_rows, executor, out);
            }
            Frontend::Close { target, name } => self.close(*target, name, out),
            Frontend::Sync => self.sync(out),
            // Nothing here buffers, so there is nothing for `Flush` to push.
            Frontend::Flush | Frontend::Terminate => {}
            Frontend::Password(_) | Frontend::Unknown { .. } => {
                self.extended_failure(
                    &SqlError::ProtocolViolation(
                        "message is not valid at this point in the session".to_owned(),
                    ),
                    out,
                );
            }
        }
    }

    /// `Parse`: name a statement.
    fn parse_message(
        &mut self,
        statement: &str,
        sql: &str,
        param_types: &[u32],
        out: &mut Vec<u8>,
    ) {
        let parsed = match parse_statements(sql) {
            Ok(statements) if statements.len() > 1 => {
                // PostgreSQL refuses this: a prepared statement is one statement, and allowing two
                // would make the row description and the command tag ambiguous.
                return self.extended_failure(
                    &SqlError::Syntax {
                        message: "cannot insert multiple commands into a prepared statement"
                            .to_owned(),
                        position: None,
                        hint: None,
                    },
                    out,
                );
            }
            Ok(mut statements) => statements.pop(),
            Err(error) => return self.extended_failure(&error, out),
        };
        self.statements.insert(
            statement.to_owned(),
            Prepared {
                parsed,
                param_types: param_types.to_vec(),
                // The query string alone, with no `PREPARE` around it — which is what a real
                // server reports for a statement the protocol named (measured, `\parse`).
                source: sql.to_owned(),
                from_sql: false,
                described: None,
            },
        );
        Backend::ParseComplete.encode(out);
    }

    /// `Bind`: fix a statement's parameters into a portal.
    fn bind(
        &mut self,
        portal: &str,
        statement: &str,
        params: &[Option<Vec<u8>>],
        formats: &[i16],
        out: &mut Vec<u8>,
    ) {
        if !self.statements.contains_key(statement) {
            return self.extended_failure(
                &SqlError::InvalidSqlStatementName(statement.to_owned()),
                out,
            );
        }
        self.portals.insert(
            portal.to_owned(),
            Portal {
                statement: statement.to_owned(),
                params: params.to_vec(),
                formats: formats.to_vec(),
                delivered: 0,
            },
        );
        Backend::BindComplete.encode(out);
    }

    /// `Describe`: what a statement takes, or what a portal returns.
    ///
    /// A statement is answered with **two** messages, `ParameterDescription` then the row shape; a
    /// portal with only the row shape, since its parameters are already bound. Captured from a real
    /// server, and getting the count wrong desynchronises the client rather than merely confusing
    /// it.
    fn describe(
        &mut self,
        target: Target,
        name: &str,
        executor: &mut dyn Execute,
        out: &mut Vec<u8>,
    ) {
        let prepared = match self.prepared_for(target, name) {
            Ok(prepared) => prepared.clone(),
            Err(error) => return self.extended_failure(&error, out),
        };
        let described = match prepared.parsed.as_ref() {
            None => Described::default(),
            Some(parsed) => match executor.describe(parsed, &prepared.param_types) {
                Ok(described) => described,
                Err(error) => return self.extended_failure(&error, out),
            },
        };
        // **Kept, because this is where a statement the protocol named gets its types.** A
        // `Parse` is not analysed, so until a `Describe` arrives `pg_prepared_statements` has
        // only the OIDs the client declared to report for it; from here it has the resolved ones,
        // which is what a real server has had since the `Parse`.
        let named = match target {
            Target::Statement => Some(name.to_owned()),
            Target::Portal => self
                .portals
                .get(name)
                .map(|portal| portal.statement.clone()),
        };
        if let Some(named) = named {
            // **Compared before it is kept, never merely replaced.** libpq sends a `Describe` of
            // the portal between every `Bind` and `Execute` — `PQsendQueryGuts` has no other shape
            // — so a baseline that a `Describe` overwrote would be compared with itself one message
            // later and the statement would answer rows in a shape the client was never told
            // about. PostgreSQL raises the refusal at `Bind`, where it revalidates the cached plan;
            // here it is one message later on the same round trip, and the baseline stays what it
            // was, so the refusal does not heal until the statement is prepared again.
            let changed = self
                .statements
                .get(&named)
                .and_then(|stored| stored.described.as_ref())
                .is_some_and(|baseline| {
                    !same_row_type(baseline.fields.as_deref(), described.fields.as_deref())
                });
            if changed {
                return self.extended_failure(&SqlError::CachedPlanMustNotChangeResultType, out);
            }
            if let Some(stored) = self.statements.get_mut(&named) {
                stored.described = Some(described.clone());
            }
        }
        if target == Target::Statement {
            // Inferred, not merely echoed back: a client that declares nothing is told what the
            // statement actually needs, which is what PostgreSQL answers and what a driver builds
            // its encoder from.
            Backend::ParameterDescription(&described.parameters).encode(out);
        }
        match described.fields {
            Some(fields) => Backend::RowDescription(&fields).encode(out),
            None => Backend::NoData.encode(out),
        }
    }

    /// `Execute`: run a portal, up to `max_rows` (0 meaning all of them).
    fn execute(
        &mut self,
        portal: &str,
        max_rows: u32,
        executor: &mut dyn Execute,
        out: &mut Vec<u8>,
    ) {
        // The extended protocol's door to the same thing `run_one` does for a simple query: a
        // `SELECT … FROM pg_prepared_statements` arrives here when a client binds it.
        self.hand_prepared_to(executor);
        let Some(open) = self.portals.get(portal).cloned() else {
            return self.extended_failure(&SqlError::InvalidCursorName(portal.to_owned()), out);
        };
        let Some(prepared) = self.statements.get(&open.statement).cloned() else {
            return self.extended_failure(&SqlError::InvalidSqlStatementName(open.statement), out);
        };
        let Some(parsed) = prepared.parsed else {
            // The empty statement. Captured: it binds and describes normally and executes to this.
            Backend::EmptyQueryResponse.encode(out);
            return;
        };

        if self.status == TransactionStatus::Failed
            && !allowed_in_a_failed_transaction(parsed.class())
        {
            return self.extended_failure(&SqlError::InFailedTransaction, out);
        }

        let outcome = match parsed.class() {
            StatementClass::Begin => self.begin(&parsed, executor, out),
            StatementClass::Commit => self.commit(executor, out),
            StatementClass::Rollback => self.rollback(executor, out),
            StatementClass::Savepoint(name) => self.savepoint(name, executor),
            StatementClass::RollbackTo(name) => self.rollback_to(name, executor),
            StatementClass::Release(name) => self.release(name, executor),
            _ => revalidate(
                &parsed,
                prepared.described.as_ref(),
                &prepared.param_types,
                executor,
            )
            .and_then(|()| {
                executor.execute(
                    &parsed,
                    &Params {
                        values: &open.params,
                        formats: &open.formats,
                        declared: &prepared.param_types,
                        bound: true,
                    },
                )
            }),
        };
        // Same rule as the simple query path: what the statement remarked on goes out before what
        // it produced.
        for notice in executor.take_notices() {
            warn(&notice, out);
        }
        match outcome {
            Ok(Outcome::Rows { rows, tag, .. }) => {
                // `Execute` sends no `RowDescription`; the client already asked for it with
                // `Describe`, and sending it again would be one message too many.
                let limit = if max_rows == 0 {
                    rows.len()
                } else {
                    max_rows as usize
                };
                let remaining = rows.len().saturating_sub(open.delivered);
                let taking = limit.min(remaining);
                for row in rows.iter().skip(open.delivered).take(taking) {
                    Backend::DataRow(row).encode(out);
                }
                if let Some(open) = self.portals.get_mut(portal) {
                    open.delivered += taking;
                }
                if max_rows != 0 && taking == limit && remaining > taking {
                    // The portal is still open and the client may ask again.
                    Backend::PortalSuspended.encode(out);
                } else {
                    Backend::CommandComplete(&tag).encode(out);
                }
            }
            Ok(Outcome::Done { tag }) => Backend::CommandComplete(&tag).encode(out),
            Err(error) => self.extended_failure(&error, out),
        }
    }

    /// `Close`: forget a statement or a portal. Closing one that does not exist is not an error.
    fn close(&mut self, target: Target, name: &str, out: &mut Vec<u8>) {
        match target {
            Target::Statement => {
                self.statements.remove(name);
                // A portal outlives nothing: closing its statement closes it too.
                self.portals.retain(|_, portal| portal.statement != name);
            }
            Target::Portal => {
                self.portals.remove(name);
            }
        }
        Backend::CloseComplete.encode(out);
    }

    /// `Sync`: end the batch, clear any failure, and report readiness.
    ///
    /// The only message in the extended protocol that sends `ReadyForQuery`.
    fn sync(&mut self, out: &mut Vec<u8>) {
        self.skipping_until_sync = false;
        // An implicit transaction opened by the batch ends here; an explicit block does not.
        Backend::ReadyForQuery(self.status).encode(out);
    }

    /// One statement that is not transaction control, through the session's own store first.
    ///
    /// **Public, and the reason is that there must not be two of these.** Four statement classes
    /// have their whole effect on state that lives here and nowhere else — the three SQL
    /// prepared-statement statements and `DISCARD ALL` — so an executor arm alone would leave a
    /// pooled connection holding the statements of whoever had it last. The corpus harness
    /// (`tests/parity_harness`) drives an `Executor` directly and mirrors this dispatch by hand;
    /// before this existed it had no arm for the three, so a `PREPARE` in a captured session
    /// answered `0A000` from a node that supports it and the corpus recorded a divergence that
    /// only the harness had.
    ///
    /// Every statement passes through here, including the ones it hands straight on, because the
    /// one it hands on may be the `SELECT … FROM pg_prepared_statements` that reads what the
    /// others wrote.
    pub fn run_statement(
        &mut self,
        parsed: &Parsed,
        executor: &mut dyn Execute,
    ) -> Result<Outcome> {
        self.hand_prepared_to(executor);
        match parsed.class() {
            // **The executor clears its own first, and only a `DISCARD ALL` that succeeded
            // clears this.** The `25001` for running it inside a block is the executor's, beside
            // `CREATE DATABASE`'s, and a refused statement must leave the store alone.
            StatementClass::DiscardAll => {
                let outcome = executor.execute(parsed, &Params::NONE);
                if outcome.is_ok() {
                    self.statements.clear();
                    self.portals.clear();
                }
                outcome
            }
            StatementClass::Prepare { name, body, types } => {
                self.prepare_sql(name.clone(), body, types, parsed.text(), executor)
            }
            StatementClass::Execute { name, args } => {
                self.execute_sql(name.clone(), args.clone(), executor)
            }
            StatementClass::ExplainExecute { name, args } => {
                self.explain_execute(name.clone(), args.clone(), parsed, executor)
            }
            StatementClass::Deallocate(name) => self.deallocate_sql(name.clone()),
            _ => executor.execute(parsed, &Params::NONE),
        }
    }

    /// Hands the executor what `pg_prepared_statements` should report, and does it now.
    ///
    /// **Rebuilt per statement rather than kept in step.** The store here is the only one; this
    /// is a copy taken at the one moment it can be read, which is the difference between a copy
    /// and a mirror. Two statements' worth of names is a handful of small strings, and a session
    /// with a thousand prepared statements has a thousand of them either way.
    fn hand_prepared_to(&self, executor: &mut dyn Execute) {
        executor.remember_prepared(
            self.statements
                .iter()
                .map(|(name, prepared)| crate::session::PreparedStatement {
                    name: name.clone(),
                    statement: prepared.source.clone(),
                    // The resolved types where analysis has happened, and the client's own
                    // declaration where it has not — never an invented empty list, because `{}`
                    // is what a statement that genuinely takes no parameters prints.
                    parameters: prepared.described.as_ref().map_or_else(
                        || prepared.param_types.clone(),
                        |described| described.parameters.clone(),
                    ),
                    results: prepared.described.as_ref().and_then(|described| {
                        described
                            .fields
                            .as_ref()
                            .map(|fields| fields.iter().map(|field| field.type_oid).collect())
                    }),
                    from_sql: prepared.from_sql,
                })
                .collect(),
        );
    }

    fn prepared_for(&self, target: Target, name: &str) -> Result<&Prepared> {
        match target {
            Target::Statement => self
                .statements
                .get(name)
                .ok_or_else(|| SqlError::InvalidSqlStatementName(name.to_owned())),
            Target::Portal => {
                let portal = self
                    .portals
                    .get(name)
                    .ok_or_else(|| SqlError::InvalidCursorName(name.to_owned()))?;
                self.statements
                    .get(&portal.statement)
                    .ok_or_else(|| SqlError::InvalidSqlStatementName(portal.statement.clone()))
            }
        }
    }

    /// A failure in the extended protocol: report it, then go quiet until `Sync`.
    fn extended_failure(&mut self, error: &SqlError, out: &mut Vec<u8>) {
        self.fail(error, out);
        self.skipping_until_sync = true;
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

/// Refuses a statement whose result row type has changed since it was described.
///
/// **PostgreSQL revalidates a cached plan before every execution**, and a statement whose rows no
/// longer have the shape the client was told about is `0A000 cached plan must not change result
/// type` rather than a surprise. `ActiveRecord` maps that sentence to
/// `PreparedStatementCacheExpired`, deallocates and retries; a node that answered with the new
/// shape instead would hand the adapter rows its column list does not match, silently.
///
/// Two orderings measured on PostgreSQL 19 and reproduced here:
///
/// * **The analysis error wins.** `SELECT b FROM t` after `DROP COLUMN b` is `42703`, and a
///   dropped table is `42P01` — not the cached-plan sentence. That falls out of comparing second:
///   there is nothing to compare when the statement no longer resolves.
/// * **It does not heal.** Every execution re-runs this, so the statement stays refused until it
///   is prepared again, which is exactly what the adapter's handler does.
///
/// A statement with no baseline is let through: this node analyses a `Parse` when something asks
/// rather than when it arrives, so a wire statement nobody has described has no shape to compare.
/// Every driver that reads rows sends the `Describe` first, and SQL `PREPARE` always has one.
fn revalidate(
    parsed: &Parsed,
    baseline: Option<&Described>,
    declared: &[u32],
    executor: &mut dyn Execute,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    let now = executor.describe(parsed, declared)?;
    if same_row_type(baseline.fields.as_deref(), now.fields.as_deref()) {
        Ok(())
    } else {
        Err(SqlError::CachedPlanMustNotChangeResultType)
    }
}

/// Whether two row shapes are the same one, by PostgreSQL's rule.
///
/// Name, type **and type modifier**, each measured rather than assumed: `RENAME COLUMN b TO bb`
/// refuses, so a name is part of the type; `ALTER COLUMN a TYPE bigint` refuses; and `varchar(10)`
/// widened to `varchar(20)` refuses, which is the one a comparison over type OIDs alone would let
/// through. PostgreSQL's fourth field is the collation and this node has none.
///
/// A statement that returns no rows compared against one that does is a change, which is what the
/// `Option` arms say: `SELECT` becoming `NoData` is not something a client can be handed quietly.
fn same_row_type(before: Option<&[FieldDescription]>, after: Option<&[FieldDescription]>) -> bool {
    match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.len() == after.len()
                && before.iter().zip(after).all(|(before, after)| {
                    before.name == after.name
                        && before.type_oid == after.type_oid
                        && before.type_modifier == after.type_modifier
                })
        }
        _ => false,
    }
}

/// Emits a warning or notice. Unlike a failure, the statement carries on afterwards and still
/// sends its `CommandComplete`.
fn warn(error: &SqlError, out: &mut Vec<u8>) {
    let fields = error_fields(error);
    error_message(error, &fields).encode(out);
}

/// Whether this statement is one of the two that **end** a failed transaction.
fn ends_a_transaction(class: &StatementClass) -> bool {
    matches!(class, StatementClass::Commit | StatementClass::Rollback)
}

/// Whether a failed transaction still accepts this statement.
///
/// Three, not two, and the third is the whole reason savepoints are worth having: **`ROLLBACK TO`
/// recovers an aborted block.** After an error every statement is `25P02` until the transaction
/// ends — except that one, which un-aborts it and lets the block go on and commit. Measured, and
/// it is what lets `ActiveRecord` run a test per transaction: a failing assertion does not poison
/// the rest of the block.
fn allowed_in_a_failed_transaction(class: &StatementClass) -> bool {
    ends_a_transaction(class) || matches!(class, StatementClass::RollbackTo(_))
}

#[cfg(test)]
mod tests {
    use super::{Described, Execute, Outcome, Params, Session};
    use crate::error::{Result, SqlError};
    use crate::parse::Parsed;
    use crate::pgwire::message::{FieldDescription, Frontend, Target, TransactionStatus};
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
        /// The statements the session last handed down, for `pg_prepared_statements`.
        ///
        /// **The last list, not every list.** It arrives before every statement, so appending
        /// them to `calls` would bury what those assertions are about; keeping the latest is what
        /// a reader of the view would see.
        prepared: Vec<crate::session::PreparedStatement>,
    }

    impl Fake {
        /// The one column this fake ever returns. Shared by `execute` and `describe`, because a
        /// `Describe` that disagreed with the following `Execute` would be a bug a real client
        /// would notice and these tests would not.
        fn fields() -> Vec<FieldDescription> {
            vec![FieldDescription {
                name: "?column?".to_owned(),
                table_oid: 0,
                column_id: 0,
                type_oid: 23,
                type_size: 4,
                type_modifier: -1,
                format: 0,
            }]
        }
    }

    impl Execute for Fake {
        /// Nothing registers this, so nothing can terminate it.
        fn terminated(&self) -> bool {
            false
        }

        fn remember_prepared(&mut self, statements: Vec<crate::session::PreparedStatement>) {
            self.prepared = statements;
        }

        /// Records the ending, so a test can assert the connection reached it; this fake holds no
        /// lock, no transaction and no schema.
        fn close(&mut self) {
            self.calls.push("close".to_string());
        }

        /// Records the option so a test can assert it reached the executor.
        fn set_option(&mut self, name: &str, value: &str) -> Result<()> {
            self.calls.push(format!("set {name} = {value}"));
            Ok(())
        }

        /// Records what it was asked to explain, so a test can assert the session resolved the
        /// right statement, and answers one row the way a plan does.
        fn explain_prepared(
            &mut self,
            explain: &Parsed,
            statement: &Parsed,
            _params: &Params<'_>,
        ) -> Result<Outcome> {
            self.calls.push(format!(
                "explain {} of {}",
                explain.rendered(),
                statement.rendered()
            ));
            Ok(Outcome::Rows {
                fields: vec![FieldDescription::computed(
                    "QUERY PLAN",
                    crate::value::ColumnType::Text,
                )],
                rows: vec![vec![Some(b"Result".to_vec())]],
                tag: "EXPLAIN".to_owned(),
            })
        }

        fn describe(&mut self, parsed: &Parsed, declared: &[u32]) -> Result<Described> {
            self.calls.push(format!("describe {}", parsed.rendered()));
            Ok(Described {
                parameters: declared.to_vec(),
                fields: (self.rows != 0).then(Fake::fields),
            })
        }

        fn execute(&mut self, parsed: &Parsed, params: &Params<'_>) -> Result<Outcome> {
            self.calls.push(format!(
                "execute {}{}",
                parsed.rendered(),
                if params.values.is_empty() {
                    String::new()
                } else {
                    format!(" with {} parameters", params.values.len())
                }
            ));
            if let Some(error) = self.fail.take() {
                return Err(error);
            }
            if self.rows == 0 {
                return Ok(Outcome::done("SELECT 0"));
            }
            let fields = Fake::fields();
            let rows = (0..self.rows)
                .map(|n| vec![Some(n.to_string().into_bytes())])
                .collect();
            Ok(Outcome::Rows {
                fields,
                rows,
                tag: format!("SELECT {}", self.rows),
            })
        }

        fn begin(&mut self, read_only: bool) -> Result<()> {
            self.calls
                .push(if read_only { "begin ro" } else { "begin" }.to_owned());
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
            fail_commit: Some(SqlError::UniqueViolation {
                constraint: "g_pkey".into(),
                key: None,
            }),
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

    // --- the extended protocol ---
    //
    // Each of these mirrors a sequence captured from PostgreSQL 19beta1 by a raw protocol client,
    // which is the only way to see what a server does with messages sent *after* a failure and
    // before a Sync.

    fn parse_msg(name: &str, sql: &str) -> Frontend {
        Frontend::Parse {
            statement: name.to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        }
    }

    fn bind_msg(portal: &str, statement: &str) -> Frontend {
        Frontend::Bind {
            portal: portal.to_owned(),
            statement: statement.to_owned(),
            param_formats: Vec::new(),
            params: Vec::new(),
            result_formats: Vec::new(),
        }
    }

    fn execute_msg(portal: &str, max_rows: u32) -> Frontend {
        Frontend::Execute {
            portal: portal.to_owned(),
            max_rows,
        }
    }

    /// Drives a batch of messages and returns everything the session answered with.
    fn batch(session: &mut Session, fake: &mut Fake, messages: &[Frontend]) -> Vec<u8> {
        let mut out = Vec::new();
        for message in messages {
            session.handle(message, fake, &mut out);
        }
        out
    }

    /// Captured: `Parse` `Bind` `Execute` `Sync` answers `1` `2` `D...` `C` `Z`.
    #[test]
    fn a_whole_extended_batch_answers_in_order() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 2,
            ..Fake::default()
        };
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("", "SELECT a FROM g"),
                bind_msg("", ""),
                execute_msg("", 0),
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "12DDCZ");
    }

    /// The rule this unit exists for, and the one a specification alone gets wrong. Captured: a
    /// failed `Parse` answers `E` immediately, the `Bind` and `Execute` that follow answer
    /// **nothing at all**, and `Sync` alone sends `ReadyForQuery`.
    #[test]
    fn after_a_failure_everything_is_discarded_until_sync() {
        let mut session = Session::new();
        let mut fake = Fake::default();

        let mut out = Vec::new();
        session.handle(&parse_msg("", "SELCT 1"), &mut fake, &mut out);
        assert_eq!(tags(&out), "E", "the failure is reported at once");

        let mut after = Vec::new();
        session.handle(&bind_msg("", ""), &mut fake, &mut after);
        session.handle(&execute_msg("", 0), &mut fake, &mut after);
        session.handle(
            &Frontend::Describe {
                target: Target::Portal,
                name: String::new(),
            },
            &mut fake,
            &mut after,
        );
        assert!(
            after.is_empty(),
            "messages after a failure must produce no bytes at all, not an error each: \
             answering them would put extra messages in the stream and desynchronise the client"
        );

        let mut synced = Vec::new();
        session.handle(&Frontend::Sync, &mut fake, &mut synced);
        assert_eq!(tags(&synced), "Z", "only Sync reports readiness");

        // And the session is usable again.
        let reply = batch(
            &mut session,
            &mut fake,
            &[parse_msg("", "SELECT 1"), Frontend::Sync],
        );
        assert_eq!(tags(&reply), "1Z");
    }

    /// Captured: describing a *statement* sends two messages, `t` then the row shape; describing a
    /// *portal* sends only the row shape, because its parameters are already bound. One message too
    /// many here desynchronises the client rather than merely confusing it.
    #[test]
    fn describing_a_statement_and_a_portal_differ_by_one_message() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 1,
            ..Fake::default()
        };
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("st", "SELECT a FROM g"),
                Frontend::Describe {
                    target: Target::Statement,
                    name: "st".to_owned(),
                },
                bind_msg("po", "st"),
                Frontend::Describe {
                    target: Target::Portal,
                    name: "po".to_owned(),
                },
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "1tT2TZ");
    }

    /// A statement that returns nothing is `NoData`, not an empty `RowDescription`.
    #[test]
    fn describing_a_statement_with_no_rows_is_nodata() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("st", "SELECT 1"),
                Frontend::Describe {
                    target: Target::Statement,
                    name: "st".to_owned(),
                },
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "1tnZ");
    }

    /// Captured: `Execute` with a row limit answers `s` `PortalSuspended` and not
    /// `CommandComplete`, and the portal stays open so the client may ask for the rest.
    #[test]
    fn a_row_limit_suspends_the_portal_and_a_second_execute_finishes_it() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 3,
            ..Fake::default()
        };
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("", "SELECT a FROM g"),
                bind_msg("", ""),
                execute_msg("", 2),
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "12DDsZ", "two rows, then suspended");

        // The portal resumes where it left off rather than starting again.
        let reply = batch(
            &mut session,
            &mut fake,
            &[execute_msg("", 0), Frontend::Sync],
        );
        assert_eq!(tags(&reply), "DCZ", "the third row, then the tag");
    }

    /// A limit that the result does not reach completes normally: suspending there would leave the
    /// client waiting for rows that do not exist.
    #[test]
    fn a_row_limit_larger_than_the_result_completes_normally() {
        let mut session = Session::new();
        let mut fake = Fake {
            rows: 1,
            ..Fake::default()
        };
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("", "SELECT a FROM g"),
                bind_msg("", ""),
                execute_msg("", 10),
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "12DCZ");
    }

    /// Captured: binding a statement that was never parsed is `26000`, and then the batch goes
    /// quiet until `Sync` like any other failure.
    #[test]
    fn binding_a_statement_that_does_not_exist_is_26000() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let mut out = Vec::new();
        session.handle(&bind_msg("", "nope"), &mut fake, &mut out);
        let framed = messages(&out);
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::INVALID_SQL_STATEMENT_NAME)
        );
        assert_eq!(
            field(&framed[0].1, b'M').as_deref(),
            Some("prepared statement \"nope\" does not exist")
        );
    }

    /// Captured: the empty statement parses, binds, and executes to `EmptyQueryResponse`; then
    /// `Close` acknowledges. A driver that probes the connection with an empty statement must not
    /// be met with an error.
    #[test]
    fn the_empty_statement_parses_binds_and_executes_to_an_empty_response() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("e", ""),
                bind_msg("", "e"),
                execute_msg("", 0),
                Frontend::Close {
                    target: Target::Statement,
                    name: "e".to_owned(),
                },
                Frontend::Sync,
            ],
        );
        assert_eq!(tags(&reply), "12I3Z");
    }

    /// A prepared statement is one statement. Two would make the row description and the command
    /// tag ambiguous, and PostgreSQL refuses it for that reason.
    #[test]
    fn a_prepared_statement_may_not_hold_two_statements() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let mut out = Vec::new();
        session.handle(&parse_msg("", "SELECT 1; SELECT 2"), &mut fake, &mut out);
        let framed = messages(&out);
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::SYNTAX_ERROR)
        );
        assert_eq!(
            field(&framed[0].1, b'M').as_deref(),
            Some("syntax error: cannot insert multiple commands into a prepared statement")
        );
    }

    /// Closing a statement closes the portals built from it: a portal outliving its statement
    /// would be executable against something that no longer exists.
    #[test]
    fn closing_a_statement_closes_its_portals() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        batch(
            &mut session,
            &mut fake,
            &[parse_msg("st", "SELECT 1"), bind_msg("po", "st")],
        );
        let mut out = Vec::new();
        session.handle(
            &Frontend::Close {
                target: Target::Statement,
                name: "st".to_owned(),
            },
            &mut fake,
            &mut out,
        );
        assert_eq!(tags(&out), "3");

        let mut after = Vec::new();
        session.handle(&execute_msg("po", 0), &mut fake, &mut after);
        let framed = messages(&after);
        assert_eq!(
            field(&framed[0].1, b'C').as_deref(),
            Some(sqlstate::INVALID_CURSOR_NAME),
            "the portal went with its statement"
        );
    }

    /// The extended protocol's failure state and the transaction's are separate things. An error
    /// inside a block sets both: quiet until `Sync`, and `E` in the `ReadyForQuery` that `Sync`
    /// sends.
    #[test]
    fn a_failure_inside_a_transaction_reports_e_at_the_next_sync() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        run(&mut session, "BEGIN", &mut fake);
        assert_eq!(session.status(), TransactionStatus::InTransaction);

        fake.fail = Some(SqlError::UndefinedTable("nope".into()));
        let reply = batch(
            &mut session,
            &mut fake,
            &[
                parse_msg("", "SELECT * FROM nope"),
                bind_msg("", ""),
                execute_msg("", 0),
                Frontend::Sync,
            ],
        );
        // Parse and Bind succeed; Execute fails; Sync reports the failed block.
        assert_eq!(tags(&reply), "12EZ");
        let framed = messages(&reply);
        assert_eq!(framed[3].1, b"E", "ReadyForQuery must say the block failed");
        assert_eq!(session.status(), TransactionStatus::Failed);
    }

    /// A simple `Query` in the middle of a session clears the extended protocol's failure state,
    /// because it carries its own `ReadyForQuery` and ends the batch by definition.
    #[test]
    fn a_simple_query_ends_a_failed_extended_batch() {
        let mut session = Session::new();
        let mut fake = Fake::default();
        let mut out = Vec::new();
        session.handle(&parse_msg("", "SELCT 1"), &mut fake, &mut out);
        session.handle(&Frontend::Sync, &mut fake, &mut out);

        let reply = batch(
            &mut session,
            &mut fake,
            &[Frontend::Query("SELECT 1".to_owned())],
        );
        assert_eq!(tags(&reply), "CZ");
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
