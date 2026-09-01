//! Statements, in types this crate owns.
//!
//! Everything the executor runs is one of these. They are produced by [`crate::parse`], which is
//! the only module allowed to name a `sqlparser` type (`docs/adr/0014-sqlparser.md`), and they are
//! deliberately *smaller* than the AST they come from: a lowered statement holds what phase 6a
//! executes and nothing else.
//!
//! # Lowering rejects, it does not ignore
//!
//! That smallness is the risk. An AST field this crate does not read is a clause the user wrote
//! and the server did not honour, and executing `CREATE TEMPORARY TABLE t` as a permanent table is
//! a worse failure than refusing it — the user is not told, and the next session finds a table it
//! did not expect.
//!
//! So the lowering in `parse.rs` names every clause it cannot honour and answers `0A000
//! feature_not_supported` (contract C2), and the tests for it are written from that side: for each
//! unsupported clause, a statement that carries it and the assertion that the clause's own name
//! comes back. A silently dropped clause is the defect class this module exists to make
//! impossible.

mod ddl;
mod dml;
mod expr;
mod query;
mod session;
mod time_machine;

pub use crate::catalog::Identity;
pub use ddl::{
    AlterTable, AlterTableAction, Column, CreateIndex, CreateTable, DropIndex, DropTable,
    UniqueConstraint, index_name, primary_key_name, sequence_name, unique_constraint_name,
};
pub use dml::{Delete, Insert, Returning, Update};
pub use expr::{AggregateCall, AggregateFunc, BinaryOp, Expr, Literal};
pub use query::{AggregateSpec, Join, Node, OrderItem, Probe, Select, SelectItem, SortKey};
pub use session::SessionStatement;
pub use time_machine::TimeMachineVerb;

/// One statement, lowered.
///
/// Transaction control is not here: `BEGIN`, `COMMIT` and `ROLLBACK` move the status a client sees
/// in every `ReadyForQuery` and so belong to the session, which handles them before the executor
/// is reached (`crate::pgwire::session`).
///
/// Not `Eq`: an expression can hold a float literal, and two of those are compared the way floats
/// are compared everywhere else in this crate rather than by an equality that pretends `NaN`
/// equals itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `CREATE INDEX`, and the `UNIQUE` variant.
    CreateIndex(CreateIndex),
    /// `DROP INDEX`.
    DropIndex(DropIndex),
    /// `ALTER TABLE`, of which only `ADD COLUMN` is executed.
    AlterTable(AlterTable),
    /// `INSERT`.
    Insert(Insert),
    /// `SELECT`.
    ///
    /// Boxed, and it is the only statement that is: with `DISTINCT`, `GROUP BY` and `HAVING` on it
    /// a `Select` is several times the size of every other variant, and an unboxed one would make
    /// every `Statement` in the crate — including a `COMMIT` — that big.
    Select(Box<Select>),
    /// `UPDATE`.
    Update(Update),
    /// `DELETE`.
    Delete(Delete),
    /// `EXPLAIN`, and the statement it is about. The inner statement is planned and described,
    /// never run.
    Explain(Box<Statement>),
    /// `SET`, `SHOW`, `RESET` — the statements that change the session rather than the store.
    /// Run outside any transaction, because one of them replaces the transaction itself.
    Session(SessionStatement),
    /// A time-machine verb, each spelled as the function call PostgreSQL parses
    /// (`docs/adr/0021-time-machine.md` Decision 3).
    TimeMachine(TimeMachineVerb),
}

impl Statement {
    /// Whether running this statement writes the catalog.
    ///
    /// The executor asks so that every later lookup in the same transaction reads through the
    /// shared cache instead of filling it: after this statement the transaction can see its own
    /// uncommitted DDL, and nothing uncommitted may be published to the node (`crate::catalog`).
    /// `EXPLAIN` is false because it runs nothing.
    #[must_use]
    pub fn writes_catalog(&self) -> bool {
        matches!(
            self,
            Statement::CreateTable(_)
                | Statement::DropTable(_)
                | Statement::CreateIndex(_)
                | Statement::DropIndex(_)
        )
    }

    /// The statement's name, when it is a `CONCURRENTLY` form that may not run inside a
    /// transaction block — PostgreSQL's `25001`, captured.
    ///
    /// Both forms, for one reason: a concurrent change is *many* transactions, so it cannot be
    /// part of one, and a block that could roll it back would be a block that could roll back half
    /// a schema change.
    #[must_use]
    pub fn concurrently(&self) -> Option<&'static str> {
        match self {
            Statement::CreateIndex(create) if create.concurrently => {
                Some("CREATE INDEX CONCURRENTLY")
            }
            Statement::DropIndex(drop) if drop.concurrently => Some("DROP INDEX CONCURRENTLY"),
            _ => None,
        }
    }

    /// The command PostgreSQL names when refusing this statement in a read-only transaction, or
    /// `None` when it writes nothing.
    ///
    /// It is the *command*, not the tag: PostgreSQL's message is `cannot execute INSERT in a
    /// read-only transaction`, and it names what the user typed so that one statement out of a
    /// block can be identified. `EXPLAIN` runs nothing at all, so it is allowed either way — which
    /// is how a user reading the past can still ask what a write *would* have done.
    #[must_use]
    pub fn write_command(&self) -> Option<&'static str> {
        match self {
            Statement::Insert(_) => Some("INSERT"),
            Statement::Update(_) => Some("UPDATE"),
            Statement::Delete(_) => Some("DELETE"),
            Statement::CreateTable(_) => Some("CREATE TABLE"),
            Statement::DropTable(_) => Some("DROP TABLE"),
            Statement::CreateIndex(_) => Some("CREATE INDEX"),
            Statement::DropIndex(_) => Some("DROP INDEX"),
            Statement::AlterTable(_) => Some("ALTER TABLE"),
            // **Not writes of this statement's transaction.** A checkpoint's record is written in
            // a present-time transaction of its own (`crate::exec::verbs`), precisely so that the
            // moment a user most wants to name — the one they are reading — is one they can name.
            // Refusing them here would make a checkpoint of the past impossible.
            // A schema step writes the catalog, and at a past snapshot that is refused like any
            // other write. The checkpoint verbs are not writes of *this* transaction — they use
            // one of their own (`crate::exec::verbs`) — but a schema step is a real DDL move and
            // has no business happening under a read of the past.
            Statement::TimeMachine(TimeMachineVerb::SchemaStep { .. }) => Some("esker_schema_step"),
            // A flashback is a write in the plainest sense — it is the *point* of it — so at a past
            // snapshot it is refused like any other. Flashing back while reading the past would be
            // writing the present from a transaction that may not write.
            Statement::TimeMachine(TimeMachineVerb::Flashback { .. }) => Some("esker_flashback"),
            Statement::TimeMachine(_)
            | Statement::Select(_)
            | Statement::Explain(_)
            | Statement::Session(_) => None,
        }
    }

    /// The command tag a successful run reports, for statements whose tag does not carry a count.
    ///
    /// PostgreSQL's tags are part of the compatibility surface: `psql` prints them and scripts
    /// branch on them. The ones with counts (`INSERT 0 3`, `SELECT 5`) are built by the executor,
    /// which is the only thing that knows the count.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Statement::CreateTable(_) => "CREATE TABLE",
            Statement::DropTable(_) => "DROP TABLE",
            Statement::CreateIndex(_) => "CREATE INDEX",
            Statement::DropIndex(_) => "DROP INDEX",
            Statement::AlterTable(_) => "ALTER TABLE",
            // Neither of these uses this: their tags carry a count, which only the executor knows.
            Statement::Insert(_) => "INSERT",
            Statement::Select(_) => "SELECT",
            Statement::Update(_) => "UPDATE",
            Statement::Delete(_) => "DELETE",
            Statement::Explain(_) => "EXPLAIN",
            Statement::Session(session) => session.tag(),
            Statement::TimeMachine(verb) => verb.tag(),
        }
    }
}
