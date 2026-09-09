//! The fixpoint a `WITH RECURSIVE` is, and the two bounds that keep it one.
//!
//! Every other `WITH` item is **inlined** — `crate::plan::cte` substitutes the body for the name —
//! and a body that names itself cannot be, because substituting it never terminates. So this is a
//! second evaluation model beside the one the rest of the crate uses: run the non-recursive term
//! once, then run the recursive term repeatedly with the previous round's rows standing in for the
//! CTE's name, until a round produces nothing.
//!
//! Measured in `tests/captures/pg19_recursive_cte.txt`.

use super::cursor::{Cursor, Settings};
use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::plan::Node;
use crate::value::Datum;

/// How many rounds a recursion may take before it is refused.
///
/// **A bound that is reached raises**; it never truncates and never loops (invariant 9 in spirit).
/// The number is a depth, not a size: a thousand rounds is deeper than any hierarchy the suite
/// walks and shallow enough that a runaway is caught in milliseconds rather than in swap.
const MAX_ROUNDS: usize = 1_000;

/// How many rows the whole recursion may accumulate before it is refused.
///
/// The other half of the same bound, because a recursion can grow without getting deeper: one
/// round that produces a million rows is as fatal as a million rounds.
const MAX_ROWS: usize = 1_000_000;

/// Runs `seed`, then `step` against its own output until a round adds nothing.
///
/// `distinct` is `UNION` rather than `UNION ALL`. **The dedup is against everything already
/// produced**, not against the last round, which is what makes `UNION` a termination rule:
/// `SELECT 1 UNION SELECT 1 FROM n` ends because the second round's `1` is not new. Measured; a
/// dedup against the previous round alone would run forever on that statement.
pub(super) fn run(
    txn: &dyn Txn,
    tenant: u64,
    settings: Settings<'_>,
    seed: &Node,
    step: &Node,
    distinct: bool,
) -> Result<Vec<Vec<Datum>>> {
    let mut produced: Vec<Vec<Datum>> = Vec::new();
    // What a row has to match to be a duplicate. Only built for `UNION`, because `UNION ALL` never
    // asks and the comparison is the expensive half.
    let mut already: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    let mut working = Vec::new();
    let mut cursor = Cursor::open(txn, tenant, settings, seed)?;
    while let Some(row) = cursor.next()? {
        if distinct && !already.insert(fingerprint(&row)) {
            continue;
        }
        working.push(row.clone());
        produced.push(row);
        refuse_if_too_many(produced.len())?;
    }
    drop(cursor);

    for round in 0..MAX_ROUNDS {
        if working.is_empty() {
            return Ok(produced);
        }
        // The step, with this round's rows where the CTE named itself. Cloned rather than shared
        // through a cell: a plan is data here, and a round that owns its own copy is a round that
        // can be printed.
        let mut planted = step.clone();
        plant(&mut planted, &working);

        let mut next = Vec::new();
        let mut cursor = Cursor::open(txn, tenant, settings, &planted)?;
        while let Some(row) = cursor.next()? {
            if distinct && !already.insert(fingerprint(&row)) {
                continue;
            }
            next.push(row.clone());
            produced.push(row);
            refuse_if_too_many(produced.len())?;
        }
        drop(cursor);
        working = next;
        // Only to make the bound's message say which one was hit.
        if round + 1 == MAX_ROUNDS && !working.is_empty() {
            return Err(SqlError::ConfigurationLimitExceeded(format!(
                "a recursive query that has not stopped after {MAX_ROUNDS} rounds needs a \
                 termination condition this server can see"
            )));
        }
    }
    Ok(produced)
}

/// `53400` naming the row bound, for a recursion that grows sideways rather than deeper.
fn refuse_if_too_many(rows: usize) -> Result<()> {
    if rows > MAX_ROWS {
        return Err(SqlError::ConfigurationLimitExceeded(format!(
            "a recursive query that has produced more than {MAX_ROWS} rows needs more memory \
             than this server will use for one query"
        )));
    }
    Ok(())
}

/// Puts this round's rows into every working table in the step.
///
/// There is exactly one — a second recursive reference is `42P19` and is refused where the body is
/// split — but the walk is total anyway, because a node that held one and was not listed here
/// would read as an empty relation and answer a wrong number of rows rather than an error.
fn plant(node: &mut Node, rows: &[Vec<Datum>]) {
    if let Node::WorkingTable { rows: slot, .. } = node {
        *slot = rows.to_vec();
        return;
    }
    for child in node.children_mut() {
        plant(child, rows);
    }
}

/// What makes two rows the same row for `UNION`'s dedup.
///
/// The rendered bytes of each datum, length-prefixed so that `('a', 'bc')` and `('ab', 'c')` do not
/// collide, and with NULL distinguished from an empty string. **NULL equals NULL here**, which it
/// does nowhere else in SQL and which a set operation measured (`tests/set_operation.rs`).
fn fingerprint(row: &[Datum]) -> Vec<u8> {
    let mut out = Vec::new();
    for datum in row {
        match crate::value::PgDatum::to_text(datum) {
            None => out.push(0),
            Some(text) => {
                out.push(1);
                out.extend_from_slice(&(text.len() as u64).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
    out
}
