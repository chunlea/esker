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

pub mod cte;
mod ddl;
mod dml;
mod expr;
mod query;
pub mod routing;
mod session;
mod subquery;
mod time_machine;

pub use crate::catalog::Identity;
pub use crate::catalog::pg_catalog::CatalogView;
pub use ddl::{
    AddValuePosition, AlterIndexRename, AlterSchemaRename, AlterTable, AlterTableAction, AlterType,
    AlterTypeAction, Column, ColumnDefault, Comment, CommentObject, CreateDatabase,
    CreateExtension, CreateFunction, CreateIndex, CreateMaterializedView, CreateRole, CreateSchema,
    CreateSequence, CreateTable, CreateTableAs, CreateTrigger, CreateType, CreateView,
    DropDatabase, DropExtension, DropFunction, DropIndex, DropMaterializedView, DropRole,
    DropSchema, DropSequence, DropTable, DropTrigger, DropType, DropView, ForeignKey, IndexKeyPart,
    KeyPartName, PartitionSpec, RangeEnd, RefreshMaterializedView, Truncate, UniqueConstraint,
    choose_relation_name, foreign_key_name, index_name, index_name_addition, make_object_name,
    primary_key_name, sequence_name, unique_constraint_name,
};
pub use dml::{ConflictAction, Delete, Insert, OnConflict, Returning, Update};
pub use expr::{
    AdvisoryCall, AggregateCall, AggregateFunc, ArithOp, BinaryOp, CaseBranch, CatalogFunc,
    CatalogFuncCall, Expr, Literal, ScalarFunc, SequenceCall, SequenceFunc, UuidFunc,
    current_setting_text, like_matches, regex_operator,
};
pub use query::{
    AggregateSpec, Join, JoinKind, LockStrength, LockWait, Locking, Node, OrderItem, Probe, Select,
    SelectItem, SortKey, TableFunction, TableRef, ValuesList,
};
pub use routing::{Columnar, Decision, Engine, Reason, Setting};
pub use session::{DiscardTarget, SessionStatement};
pub use subquery::{Derived, SubqueryExpr, SubqueryKind};
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
    /// `DO $$ BEGIN RAISE NOTICE | WARNING '<text>'; END $$` — the suite's *other* `DO` body.
    ///
    /// `postgresql_adapter_test.rb` raises one to exercise `db_warnings_action`, so what the test
    /// needs is the message reaching the client at the severity that was written. There is no
    /// PL/pgSQL here: one `RAISE` of a literal is a statement, and every other body is refused by
    /// name (`crate::parse::strip_do_raise`).
    Raise {
        /// The text between the quotes, with `''` already unescaped.
        message: String,
        /// `NOTICE` or `WARNING`. `INFO`, `LOG` and `DEBUG` have no severity on this wire and are
        /// refused by name rather than downgraded into one that would print the wrong word.
        severity: crate::error::Severity,
    },
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `DROP SEQUENCE [IF EXISTS] s [CASCADE]` — the sequence, its name, its counter, and the
    /// column default that *is* it.
    DropSequence(DropSequence),
    /// `CREATE SEQUENCE`.
    CreateSequence(CreateSequence),
    /// `DROP FUNCTION`.
    DropFunction(DropFunction),
    /// `CREATE [OR REPLACE] FUNCTION`.
    CreateFunction(CreateFunction),
    /// `CREATE TRIGGER`.
    CreateTrigger(CreateTrigger),
    /// `DROP TRIGGER`.
    DropTrigger(DropTrigger),
    /// `CREATE EXTENSION [IF NOT EXISTS] name` — a catalog write and nothing else here: it records
    /// that the extension is installed, and what an extension *carries* is either already in this
    /// build or is why the name is not available.
    CreateExtension(CreateExtension),
    /// `DROP EXTENSION [IF EXISTS] <name> [CASCADE]`, which is what the suite's teardown sends.
    DropExtension(DropExtension),
    /// `ALTER INDEX [IF EXISTS] <name> RENAME TO <name>`.
    AlterIndexRename(AlterIndexRename),
    /// `CREATE SCHEMA` — a second namespace, which is a catalog object like any other here.
    CreateSchema(CreateSchema),
    /// `CREATE ROLE` / `CREATE USER`.
    CreateRole(CreateRole),
    /// `CREATE VIEW` — a stored `SELECT`, expanded where it is read.
    CreateView(CreateView),
    /// `DROP VIEW`.
    DropView(DropView),
    /// `CREATE MATERIALIZED VIEW` — a table that carries the `SELECT` its rows came from
    /// ([ADR 0064](../../../docs/adr/0064-a-materialized-view-is-a-table-whose-rows-are-recomputed.md)).
    CreateMaterializedView(CreateMaterializedView),
    /// `CREATE TABLE … AS <query>` — a table whose columns are typed from the query's plan.
    CreateTableAs(CreateTableAs),
    /// `REFRESH MATERIALIZED VIEW` — the statement that recomputes those rows.
    RefreshMaterializedView(RefreshMaterializedView),
    /// `DROP MATERIALIZED VIEW`.
    DropMaterializedView(DropMaterializedView),
    /// `TRUNCATE [TABLE] …` — every row of the tables named, and their sequences only if asked.
    Truncate(Truncate),
    /// `CREATE DATABASE` — a second **tenant**, which is what a database is (ADR 0052).
    CreateDatabase(CreateDatabase),
    /// `DROP DATABASE`, which takes everything that tenant held with it.
    DropDatabase(DropDatabase),
    /// `DROP SCHEMA [CASCADE]`.
    DropSchema(DropSchema),
    /// `DROP ROLE` / `DROP USER`.
    DropRole(DropRole),
    /// `ALTER SCHEMA … RENAME TO …`.
    AlterSchemaRename(AlterSchemaRename),
    /// `CREATE INDEX`, and the `UNIQUE` variant.
    CreateIndex(CreateIndex),
    /// `DROP INDEX`.
    DropIndex(DropIndex),
    /// `COMMENT ON TABLE | COLUMN | INDEX`.
    Comment(Comment),
    /// `CREATE TYPE`.
    CreateType(CreateType),
    /// `ALTER TYPE` — rename the type, add a label, or rename a label.
    AlterType(AlterType),
    /// `DROP TYPE`.
    DropType(DropType),
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
    /// `EXPLAIN`, and the statement it is about.
    ///
    /// The flag is `ANALYZE`, which **runs** the statement — that is what the word means on a real
    /// server, and it is why it stays refused for everything that writes. For a `SELECT` it is
    /// what puts the `ScanStats` a columnar answer carries into the plan
    /// (`docs/plans/phase-10-routing.md` U3).
    Explain(Box<Statement>, bool),
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
    ///
    /// # This is a deny-list, and it is one on purpose
    ///
    /// It was an allow-list of five statements — `CREATE`/`DROP TABLE`, `CREATE`/`DROP INDEX` and
    /// `COMMENT` — and **`ALTER TABLE` was not among them**. So every `ALTER` published its
    /// transaction's uncommitted table to a cache the whole node reads, and a transaction that
    /// then rolled back left it there. The cache is keyed by catalog *version*, and a version is
    /// not a transaction identity: the next transaction to reach that same number was handed the
    /// rolled-back one's table. Two `ALTER`s were needed to line the numbers up, which is why the
    /// bug reproduced at exactly two and not at one or three
    /// (`tests/rolled_back_ddl_cache.rs`).
    ///
    /// Naming what does **not** write the catalog inverts the cost of forgetting. A new statement
    /// left off an allow-list is a silent wrong answer; left off this list it is a cache this
    /// transaction stops using, which costs a lookup and nothing else. Every arm below is a
    /// statement that reads or writes *rows*, or touches no stored state at all.
    #[must_use]
    pub fn writes_catalog(&self) -> bool {
        !matches!(
            self,
            // Rows, not definitions. These are the hot path, and marking one of them true would
            // turn the cache off for the rest of every transaction that wrote a row.
            Statement::Insert(_)
                | Statement::Select(_)
                | Statement::Update(_)
                | Statement::Delete(_)
                // Runs nothing.
                | Statement::Explain(..)
                // A message to the client, and `SET`/`SHOW`, which are session state and not
                // catalog state — `crate::parameter` owns them and no cache reads them.
                | Statement::Raise { .. }
                | Statement::Session(_)
                // Reads of history (`docs/adr/0021-time-machine.md`).
                | Statement::TimeMachine(_)
        )
    }

    /// The statement's name, when it is a `CONCURRENTLY` form that may not run inside a
    /// transaction block — PostgreSQL's `25001`, captured.
    ///
    /// Four statements, for one reason: a concurrent change is *many* transactions and a database
    /// is state outside every one of them, so neither can be part of a block — and a block that
    /// could roll either back would be a block that could roll back half a schema change.
    #[must_use]
    pub fn refused_in_a_transaction_block(&self) -> Option<&'static str> {
        match self {
            Statement::CreateIndex(create) if create.concurrently => {
                Some("CREATE INDEX CONCURRENTLY")
            }
            Statement::DropIndex(drop) if drop.concurrently => Some("DROP INDEX CONCURRENTLY"),
            Statement::CreateDatabase(_) => Some("CREATE DATABASE"),
            Statement::DropDatabase(_) => Some("DROP DATABASE"),
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
            Statement::Truncate(_) => Some("TRUNCATE"),
            Statement::Insert(_) => Some("INSERT"),
            Statement::Update(_) => Some("UPDATE"),
            Statement::Delete(_) => Some("DELETE"),
            Statement::CreateTable(_) => Some("CREATE TABLE"),
            // A materialized view **writes rows**: creating and refreshing one both run the
            // definition and store what it produced (ADR 0064).
            Statement::CreateMaterializedView(_) => Some("CREATE MATERIALIZED VIEW"),
            Statement::CreateTableAs(_) => Some("CREATE TABLE AS"),
            Statement::RefreshMaterializedView(_) => Some("REFRESH MATERIALIZED VIEW"),
            Statement::DropMaterializedView(_) => Some("DROP MATERIALIZED VIEW"),
            // A catalog write like the rest, so a read-only or time-travelling block refuses it.
            Statement::CreateExtension(_) => Some("CREATE EXTENSION"),
            Statement::DropExtension(_) => Some("DROP EXTENSION"),
            Statement::AlterIndexRename(_) => Some("ALTER INDEX"),
            Statement::CreateSchema(_) => Some("CREATE SCHEMA"),
            Statement::CreateRole(_) => Some("CREATE ROLE"),
            Statement::DropRole(_) => Some("DROP ROLE"),
            Statement::CreateView(_) => Some("CREATE VIEW"),
            Statement::DropView(_) => Some("DROP VIEW"),
            Statement::CreateDatabase(_) => Some("CREATE DATABASE"),
            Statement::DropDatabase(_) => Some("DROP DATABASE"),
            Statement::DropSchema(_) => Some("DROP SCHEMA"),
            Statement::AlterSchemaRename(_) => Some("ALTER SCHEMA"),
            Statement::DropTable(_) => Some("DROP TABLE"),
            // A catalog write like the rest: it rewrites the table record the comment lives in.
            Statement::Comment(_) => Some("COMMENT"),
            Statement::CreateType(_) => Some("CREATE TYPE"),
            // **`ADD VALUE` may rewrite rows** — a label inserted mid-list moves the ordinal every
            // later one is stored as — so this is a write, not only a catalog change.
            Statement::AlterType(_) => Some("ALTER TYPE"),
            Statement::DropType(_) => Some("DROP TYPE"),
            Statement::DropSequence(_) => Some("DROP SEQUENCE"),
            Statement::CreateSequence(_) => Some("CREATE SEQUENCE"),
            Statement::DropFunction(_) => Some("DROP FUNCTION"),
            Statement::CreateFunction(_) => Some("CREATE FUNCTION"),
            Statement::CreateTrigger(_) => Some("CREATE TRIGGER"),
            Statement::DropTrigger(_) => Some("DROP TRIGGER"),
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
            // A `RAISE` writes nothing: it is allowed in a read-only transaction and against the
            // past, exactly as `SELECT` is.
            Statement::Raise { .. }
            | Statement::TimeMachine(_)
            | Statement::Select(_)
            | Statement::Explain(..)
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
            // The tag is the outer statement's, not the body's: a real server answers `DO`.
            Statement::Raise { .. } => "DO",
            Statement::Truncate(_) => "TRUNCATE TABLE",
            Statement::CreateTable(_) => "CREATE TABLE",
            Statement::CreateMaterializedView(_) => "CREATE MATERIALIZED VIEW",
            Statement::CreateTableAs(_) => "CREATE TABLE AS",
            Statement::RefreshMaterializedView(_) => "REFRESH MATERIALIZED VIEW",
            Statement::DropMaterializedView(_) => "DROP MATERIALIZED VIEW",
            Statement::CreateExtension(_) => "CREATE EXTENSION",
            Statement::DropExtension(_) => "DROP EXTENSION",
            Statement::AlterIndexRename(_) => "ALTER INDEX",
            Statement::CreateSchema(_) => "CREATE SCHEMA",
            Statement::CreateRole(_) => "CREATE ROLE",
            Statement::DropRole(_) => "DROP ROLE",
            Statement::DropSchema(_) => "DROP SCHEMA",
            Statement::CreateView(_) => "CREATE VIEW",
            Statement::DropView(_) => "DROP VIEW",
            Statement::CreateDatabase(_) => "CREATE DATABASE",
            Statement::DropDatabase(_) => "DROP DATABASE",
            Statement::AlterSchemaRename(_) => "ALTER SCHEMA",
            Statement::DropTable(_) => "DROP TABLE",
            // **`COMMENT`, not `COMMENT ON`** — PostgreSQL's tag is the first word alone, which
            // `psql` prints back and a script may branch on.
            Statement::Comment(_) => "COMMENT",
            Statement::CreateType(_) => "CREATE TYPE",
            Statement::AlterType(_) => "ALTER TYPE",
            Statement::DropType(_) => "DROP TYPE",
            Statement::DropSequence(_) => "DROP SEQUENCE",
            Statement::CreateSequence(_) => "CREATE SEQUENCE",
            Statement::DropFunction(_) => "DROP FUNCTION",
            Statement::CreateFunction(_) => "CREATE FUNCTION",
            Statement::CreateTrigger(_) => "CREATE TRIGGER",
            Statement::DropTrigger(_) => "DROP TRIGGER",
            Statement::CreateIndex(_) => "CREATE INDEX",
            Statement::DropIndex(_) => "DROP INDEX",
            Statement::AlterTable(_) => "ALTER TABLE",
            // Neither of these uses this: their tags carry a count, which only the executor knows.
            Statement::Insert(_) => "INSERT",
            Statement::Select(_) => "SELECT",
            Statement::Update(_) => "UPDATE",
            Statement::Delete(_) => "DELETE",
            Statement::Explain(..) => "EXPLAIN",
            Statement::Session(session) => session.tag(),
            Statement::TimeMachine(verb) => verb.tag(),
        }
    }
}
