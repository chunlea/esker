//! A stateless node that speaks PostgreSQL.
//!
//! `psql` connects here, and tables live in the `'t'` key space (`docs/DESIGN.md` §3) behind Esker
//! transactions. The node holds no state but caches, so any number of them can run behind a load
//! balancer and any one of them can be killed (`docs/DESIGN.md` §13).
//!
//! # The compatibility contract
//!
//! Every crate below this one implements a format Esker chose. This one implements a format
//! PostgreSQL chose and already shipped, so "close enough" is not a position that exists. The
//! contract has three layers, spelled out in `docs/plans/phase-6a.md` §1, and it binds from the
//! first commit even though the set of statements actually executed grows one unit at a time:
//!
//! * **C1 — syntax.** No statement PostgreSQL 19 accepts is rejected as a parse error. The layer is
//!   bounded by [`parse`]'s dependency, so its enforceable form is that no gap is *silent*: an
//!   unparseable statement goes on the gap register in the plan, never quietly into a user's error
//!   log.
//! * **C2 — honesty.** A statement that parses and that Esker cannot execute returns SQLSTATE
//!   `0A000 feature_not_supported` naming the feature, and leaves the session state machine where
//!   PostgreSQL would have left it. Never a parse error, never a wrong answer, never a panic. This
//!   is what makes a small executor safe to ship: the set of statements we execute is small, and
//!   the set we mishandle is empty.
//! * **C3 — parity.** For what is executed, behaviour matches PostgreSQL 19 exactly — value text
//!   formats, NULL semantics, SQLSTATE codes, command tags — asserted against documented behaviour
//!   rather than against our own expectations.
//!
//! # Invariants
//!
//! * **One place chooses a SQLSTATE.** Every failure is an [`error::SqlError`], and the code comes
//!   from the error, never from the call site (contract C3 is only checkable if the mapping is in
//!   one place).
//! * **The parser is contained.** `sqlparser` types appear in [`parse`] and nowhere else, which is
//!   what keeps replacing it a one-module job (`docs/adr/0014-sqlparser.md`). `tests/containment.rs`
//!   is what makes that a fact rather than an intention.
//! * **The stack is guarded before the parser is entered.** The dependency is built without its
//!   recursion protection, so [`parse::nesting_depth`] is what stands between a deeply nested
//!   statement and an aborted process (`CLAUDE.md` invariant 9).
//! * **Formats carry a version byte and are golden-tested.** Row values and catalog records are
//!   on-disk formats like any other in this project (`CLAUDE.md` invariant 2); an unknown version
//!   is a typed error.
//! * **Nothing here holds durable state.** The catalog cache is a cache, checked against a version
//!   in the store once per transaction; losing this process loses nothing.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod backend;
pub mod catalog;
pub mod error;
pub mod exec;
pub mod parse;
pub mod pgwire;
pub mod plan;
pub mod row;
pub mod sqlstate;
pub mod time_machine;
pub mod value;

pub use error::{Result, Severity, SqlError};
pub use value::{ColumnType, Datum};
