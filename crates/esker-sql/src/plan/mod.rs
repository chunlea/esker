//! Statements, in types this crate owns.
//!
//! Everything the executor runs is one of these. They are produced by [`crate::parse`], which is
//! the only file allowed to name a `sqlparser` type (`docs/adr/0014-sqlparser.md`), and they are
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

pub use ddl::{
    Column, CreateIndex, CreateTable, DropIndex, DropTable, UniqueConstraint, index_name,
    primary_key_name, unique_constraint_name,
};
pub use dml::Insert;
pub use expr::{Expr, Literal};

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
    /// `INSERT`.
    Insert(Insert),
    /// `EXPLAIN`, and the statement it is about. The inner statement is planned and described,
    /// never run.
    Explain(Box<Statement>),
}

impl Statement {
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
            // `INSERT` never uses this: its tag carries a count, which only the executor knows.
            Statement::Insert(_) => "INSERT",
            Statement::Explain(_) => "EXPLAIN",
        }
    }
}
