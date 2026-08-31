//! The time machine's verbs, run.
//!
//! Named for what [ADR 0021](../../../docs/adr/0021-time-machine.md) Decision 3 calls them, and
//! not `time_machine`, so that the module this one leans on — `crate::time_machine`, which holds
//! the value grammar, the token and the window — can be named here without qualification.
//!
//! Three of them, and what they have in common is how little they do. A checkpoint is a **number
//! with a name**: taking one writes nine bytes and touches nothing else — no snapshot, no copy, no
//! flush — because the versions it refers to are kept by retention whether anybody named it or not.
//!
//! That is also the catch, and it is why the record is a **claim to check** rather than a
//! guarantee: a checkpoint older than the travel window names history that is gone. Nothing here
//! checks it, deliberately — the check belongs where the read happens, so that a name can outlive
//! the data it points at and say so at the moment somebody tries to use it.
//!
//! # Why a checkpoint is written in a transaction of its own
//!
//! Because the most useful checkpoint is one taken *while reading the past*: "I am looking at an
//! hour ago; remember this moment as `before-the-incident`." That transaction is read-only by
//! construction ([`crate::backend::Backend::begin_at`]), so a record written inside it could never
//! commit — and refusing the statement would mean the one moment a user most wants to name is the
//! one they cannot have.
//!
//! So the *value* is the reading transaction's `start_ts` and the *write* is an ordinary
//! present-time transaction of its own — the same shape, and for the same kind of structural
//! reason, as [`Executor::next_row_id`]. The cost is stated rather than hidden: a block that rolls
//! back leaves the name behind. A name is a bookmark and not data, and a bookmark surviving a
//! cancelled read is not a correctness problem; where it would be one,
//! `esker_drop_checkpoint('<name>')` removes it.

use crate::backend::Txn;
use crate::catalog;
use crate::error::Result;
use crate::exec::{Executor, for_each_page};
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::Outcome;
use crate::plan::TimeMachineVerb;
use crate::time_machine::{check_name, render, token};
use crate::value::ColumnType;

/// Runs one verb.
pub(super) fn run(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    verb: &TimeMachineVerb,
) -> Result<Outcome> {
    match verb {
        TimeMachineVerb::ExportSnapshot { name } => export(executor, txn, name.as_deref()),
        TimeMachineVerb::DropCheckpoint { name } => drop_one(executor, name),
        TimeMachineVerb::ListCheckpoints => list(executor, txn),
    }
}

/// `pg_export_snapshot()`, and `esker_checkpoint('<name>')`.
///
/// The timestamp is **this transaction's own snapshot**, not a fresh one, and that is the whole
/// point of PostgreSQL's verb: what it exports is what the exporting transaction sees, so a second
/// session importing the token reads exactly the state the first one was reading. Allocating a new
/// timestamp here would export a moment nobody had looked at.
///
/// The anonymous form writes nothing at all — the token carries the timestamp — so it is the one
/// verb here that is free even of a record.
fn export(executor: &mut Executor, txn: &dyn Txn, name: Option<&str>) -> Result<Outcome> {
    let at = txn.start_ts();
    if let Some(name) = name {
        // **Not folded.** The name arrives as a string literal, and PostgreSQL does not case-fold
        // those; `esker_checkpoint('Nightly')` keeps its capital. Validated with the rule
        // `SET TRANSACTION SNAPSHOT` reads names by, so that whatever can be named can be
        // imported — a name too long to be a snapshot identifier would otherwise be writable and
        // then unusable.
        check_name(name)?;
        let tenant = executor.tenant;
        executor.in_its_own_transaction(|own| {
            catalog::set_checkpoint(own, tenant, name, at);
            Ok(())
        })?;
    }
    Ok(one_text("pg_export_snapshot", token(at)))
}

/// `esker_drop_checkpoint('<name>')` — forgets the name.
///
/// The versions it named are retention's business and are not touched, which is the difference
/// between forgetting a bookmark and burning the book. Dropping a name that is not there answers
/// `f` rather than raising: a client cleaning up after itself should not have to look first.
///
/// In its own transaction for the same reason [`export`] is: a session reading the past must be
/// able to tidy up a name it no longer wants.
fn drop_one(executor: &mut Executor, name: &str) -> Result<Outcome> {
    check_name(name)?;
    let tenant = executor.tenant;
    let mut existed = false;
    executor.in_its_own_transaction(|own| {
        existed = catalog::checkpoint_at(own, tenant, name)?.is_some();
        if existed {
            catalog::drop_checkpoint(own, tenant, name);
        }
        Ok(())
    })?;
    Ok(one_text(
        "esker_drop_checkpoint",
        if existed { "t" } else { "f" }.to_owned(),
    ))
}

/// `SELECT * FROM esker_checkpoints()` — the names, their tokens and their instants.
///
/// Three columns rather than one, because a name on its own does not answer the question a user
/// listing checkpoints actually has: *is this one still inside the window?* The instant is what
/// they compare against, and the token is what they paste into `SET TRANSACTION SNAPSHOT`.
///
/// Read in the statement's own transaction, unlike the two above: this writes nothing, so a
/// session reading the past sees the checkpoints **as they were at that snapshot**, which is the
/// consistent answer rather than a special case.
fn list(executor: &mut Executor, txn: &mut dyn Txn) -> Result<Outcome> {
    let tenant = executor.tenant;
    let (start, end) = catalog::checkpoint_range(tenant);
    let mut rows = Vec::new();
    for_each_page(txn, &start, &end, |_, page| {
        for (key, value) in page {
            let (name, at) = catalog::decode_checkpoint(tenant, key, value)?;
            rows.push(vec![
                Some(name.into_bytes()),
                Some(token(at).into_bytes()),
                Some(render(at).into_bytes()),
            ]);
        }
        Ok(())
    })?;
    let tag = format!("SELECT {}", rows.len());
    Ok(Outcome::Rows {
        fields: ["name", "snapshot", "at"]
            .into_iter()
            .map(|name| FieldDescription::computed(name, ColumnType::Text))
            .collect(),
        rows,
        tag,
    })
}

/// One row of one text column, which is the shape every scalar verb here returns.
fn one_text(column: &str, value: String) -> Outcome {
    Outcome::Rows {
        fields: vec![FieldDescription::computed(column, ColumnType::Text)],
        rows: vec![vec![Some(value.into_bytes())]],
        tag: "SELECT 1".to_owned(),
    }
}
