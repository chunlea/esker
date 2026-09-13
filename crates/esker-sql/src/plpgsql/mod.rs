//! PL/pgSQL, as far as the suite sends it — [ADR 0113].
//!
//! **A subset, and the subset is a census.** The Rails suite sends three `DO` bodies, two trigger
//! functions and two row triggers (`docs/plans/plpgsql-subset.md` §2), and the constructs they use
//! are the whole of what reads here: a block with declarations, `IF … THEN … END IF`, `RAISE` of a
//! literal, `SELECT … INTO`, assignment, `FOR <record> IN <query> LOOP`, `EXECUTE`, `RETURN`, and
//! any SQL statement. Every other construct PostgreSQL has is refused by name, before any of the
//! body runs.
//!
//! This module **reads** a body into a [`Block`] and runs nothing. It knows no catalog and no
//! transaction; the SQL a body holds stays source text here, and the executor is what runs a block
//! (`crate::exec`), inside the statement that reached it.
//!
//! [ADR 0113]: ../../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md

mod grammar;
mod lex;
#[cfg(test)]
mod tests;

pub use grammar::{
    Block, Context, Declaration, RaiseLevel, Statement, Target, VariableType, parse,
};
