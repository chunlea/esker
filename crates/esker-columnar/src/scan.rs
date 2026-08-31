//! Evaluating a fragment against one columnar file.
//!
//! Prune, decode, filter, aggregate — in that order, and the order is the point. Each step exists
//! to make the next one smaller, and the arithmetic ADR 0022 states its cost rule in (`rows
//! scanned × columns projected` against `rows scanned × columns stored`) is only true if both
//! reductions actually happen.
//!
//! # Pruning, and why it is sound
//!
//! Every **top-level conjunct** of the filter is tested against the statistics of the column it
//! names. Only conjunctions count: a comparison under an `OR` constrains nothing, because the
//! other branch may match anything — the same rule `esker_sql::exec::query` applies to required
//! constants, arrived at here independently and for the same reason.
//!
//! Soundness rests on one fact about M1's bounds: **truncation only ever widens them.** A
//! truncated minimum sorts at or below the true minimum and a truncated maximum at or above the
//! true maximum, so a range that excludes the whole widened interval excludes the true one. Only
//! one rule below needs more than that — `<>` can skip a stripe only when every value in it
//! equals the literal, which a widened bound cannot establish — and it asks for exact bounds.
//!
//! Nothing here concludes that a stripe *does* match. Pruning may only ever remove work, so a
//! pruner that is merely conservative is correct and one that is merely wrong is not: the
//! difference is a missing row, and `tests/scan.rs` runs every generated fragment with pruning on
//! and off and requires the same answer.
//!
//! # Only what is needed is decoded
//!
//! Not just the projection — the **union of the slots the filter, the grouping and the aggregates
//! actually name**. `SELECT count(*) FROM t` therefore decodes nothing at all and answers out of
//! the footer, which is the extreme case of the property this crate exists for.

pub mod group;

use std::collections::BTreeMap;

use crate::column::Column;
use crate::error::Result;
use crate::footer::StripeMeta;
use crate::fragment::expr::{CompareOp, Expr};
use crate::fragment::{Fragment, Output};
use crate::reader::Reader;
use crate::stats::ColumnStats;
use crate::value::{ColumnType, Value, ValueRef};

pub use group::{GroupKey, Partial};

use group::Accumulator;

/// What one evaluation touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanStats {
    /// Stripes the file has, in the range the fragment asked for.
    pub stripes_considered: u64,
    /// Stripes whose statistics did not rule them out.
    pub stripes_read: u64,
    /// Column chunks decoded.
    pub chunks_decoded: u64,
    /// Rows that reached the filter.
    pub rows_scanned: u64,
    /// Rows the filter kept.
    pub rows_matched: u64,
}

impl ScanStats {
    /// Stripes skipped without reading a byte of them.
    #[must_use]
    pub fn stripes_pruned(&self) -> u64 {
        self.stripes_considered - self.stripes_read
    }
}

/// One group of a fragment's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The grouping slots' values, in the order the fragment named them.
    pub key: Vec<Value>,
    /// One partial per aggregate, in the order the fragment named them.
    pub aggregates: Vec<Partial>,
}

/// What a fragment produced.
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentOutput {
    /// The projected columns of the rows that matched, in row order.
    Rows(Vec<Vec<Value>>),
    /// Partial aggregates, one group each, in [`crate::ValueRef::pg_cmp`] order of their keys.
    Groups(Vec<Group>),
}

/// A fragment's answer, and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct FragmentResult {
    /// The answer.
    pub output: FragmentOutput,
    /// What was read to produce it.
    pub stats: ScanStats,
}

/// How a scan is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Whether to skip stripes whose statistics rule them out.
    ///
    /// A switch rather than a constant, for the reason ADR 0022 gives for the session GUC it
    /// asks the planner to have: the two cases every such system needs it for are somebody who
    /// knows better and somebody **bisecting a wrong answer**. Pruning is the part of this scan
    /// that can lose a row, so being able to turn it off is what turns "the answer is wrong"
    /// into "the answer is wrong *because of pruning*" in one run. `tests/scan.rs` uses it to
    /// require both modes to agree over every generated fragment.
    pub prune: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { prune: true }
    }
}

/// Evaluates `fragment` against `reader`.
///
/// Every refusal happens in [`Fragment::validate`], before a byte is read, so a fragment that
/// gets past the first line here will be evaluated in full or fail on damaged data.
pub fn evaluate(reader: &Reader, fragment: &Fragment) -> Result<FragmentResult> {
    evaluate_with(reader, fragment, ScanOptions::default())
}

/// [`evaluate`], with the scan's switches exposed.
pub fn evaluate_with(
    reader: &Reader,
    fragment: &Fragment,
    options: ScanOptions,
) -> Result<FragmentResult> {
    let slots = fragment.validate(reader.schema())?;
    let needed = needed_slots(fragment, slots.len());
    let mut conjuncts = Vec::new();
    if let Some(filter) = &fragment.filter {
        filter.conjuncts(&mut conjuncts);
    }

    let mut stats = ScanStats::default();
    let mut rows = Vec::new();
    let mut groups: BTreeMap<GroupKey, Vec<Accumulator>> = BTreeMap::new();
    let limit = match &fragment.output {
        Output::Rows { limit } => *limit,
        Output::Aggregates { .. } => None,
    };

    for (index, stripe) in reader.stripes().iter().enumerate() {
        stats.stripes_considered += 1;
        if limit.is_some_and(|limit| stats.rows_matched >= limit) {
            continue;
        }
        if options.prune && !stripe_can_match(stripe, &conjuncts, fragment, &slots) {
            continue;
        }
        stats.stripes_read += 1;

        let columns = decode_needed(reader, index, fragment, &needed)?;
        stats.chunks_decoded += columns.iter().filter(|column| column.is_some()).count() as u64;
        let mut cursors: Vec<Option<crate::column::ColumnIter<'_>>> = columns
            .iter()
            .map(|column| column.as_ref().map(Column::iter))
            .collect();

        let mut row: Vec<ValueRef<'_>> = vec![ValueRef::Null; slots.len()];
        for _ in 0..stripe.rows {
            for (slot, cursor) in cursors.iter_mut().enumerate() {
                row[slot] = cursor
                    .as_mut()
                    .and_then(Iterator::next)
                    .unwrap_or(ValueRef::Null);
            }
            stats.rows_scanned += 1;
            if let Some(filter) = &fragment.filter
                && !filter.matches(&row)
            {
                continue;
            }
            stats.rows_matched += 1;

            match &fragment.output {
                Output::Rows { .. } => {
                    rows.push(
                        row.iter()
                            .zip(&slots)
                            .map(|(value, ty)| value.to_value(*ty))
                            .collect::<Result<Vec<_>>>()?,
                    );
                    if limit.is_some_and(|limit| stats.rows_matched >= limit) {
                        break;
                    }
                }
                Output::Aggregates {
                    group_by,
                    aggregates,
                } => {
                    let key = GroupKey::new(
                        group_by
                            .iter()
                            .map(|slot| row[*slot as usize].to_value(slots[*slot as usize]))
                            .collect::<Result<Vec<_>>>()?,
                    );
                    let accumulators = groups.entry(key).or_insert_with(|| {
                        aggregates
                            .iter()
                            .map(|aggregate| Accumulator::new(*aggregate, &slots))
                            .collect()
                    });
                    for accumulator in accumulators.iter_mut() {
                        accumulator.push(&row)?;
                    }
                }
            }
        }
    }

    let output = finish(fragment, rows, groups, &slots);
    Ok(FragmentResult { output, stats })
}

/// Turns what the scan accumulated into the fragment's answer.
///
/// The one subtlety is the empty grouping: a fragment with no `GROUP BY` has exactly one group
/// whether or not any row matched, because `SELECT count(*) FROM t WHERE false` answers 0 rather
/// than answering nothing. A fragment that *does* group produces no group at all in that case,
/// which is also PostgreSQL.
fn finish(
    fragment: &Fragment,
    rows: Vec<Vec<Value>>,
    mut groups: BTreeMap<GroupKey, Vec<Accumulator>>,
    slots: &[ColumnType],
) -> FragmentOutput {
    let Output::Aggregates {
        group_by,
        aggregates,
    } = &fragment.output
    else {
        return FragmentOutput::Rows(rows);
    };

    if group_by.is_empty() && groups.is_empty() {
        groups.insert(
            GroupKey::new(Vec::new()),
            aggregates
                .iter()
                .map(|aggregate| Accumulator::new(*aggregate, slots))
                .collect(),
        );
    }
    FragmentOutput::Groups(
        groups
            .into_iter()
            .map(|(key, accumulators)| Group {
                key: key.into_values(),
                aggregates: accumulators.iter().map(Accumulator::finish).collect(),
            })
            .collect(),
    )
}

/// Decodes the chunks this fragment names, and only those.
///
/// A slot nobody reads comes back `None`, and reading one would yield NULL — which the
/// differential harness catches at once, because its reference decodes every column and compares.
/// That is what lets this be an optimisation rather than something to prove separately.
fn decode_needed(
    reader: &Reader,
    stripe: usize,
    fragment: &Fragment,
    needed: &[bool],
) -> Result<Vec<Option<Column>>> {
    needed
        .iter()
        .zip(&fragment.projection)
        .map(|(wanted, column)| {
            if *wanted {
                reader.read_column(stripe, *column as usize).map(Some)
            } else {
                Ok(None)
            }
        })
        .collect()
}

/// Which projection slots something in the fragment actually names.
fn needed_slots(fragment: &Fragment, width: usize) -> Vec<bool> {
    fn mark(needed: &mut [bool], slot: u32) {
        if let Some(entry) = needed.get_mut(slot as usize) {
            *entry = true;
        }
    }

    let mut needed = vec![false; width];
    match &fragment.output {
        // Every projected column is part of the answer.
        Output::Rows { .. } => needed.iter_mut().for_each(|entry| *entry = true),
        Output::Aggregates {
            group_by,
            aggregates,
        } => {
            for slot in group_by {
                mark(&mut needed, *slot);
            }
            for aggregate in aggregates {
                if let Some(slot) = aggregate.slot() {
                    mark(&mut needed, slot);
                }
            }
        }
    }
    if let Some(filter) = &fragment.filter {
        let mut stack = vec![filter];
        while let Some(node) = stack.pop() {
            match node {
                Expr::Column(slot) => mark(&mut needed, *slot),
                Expr::Literal(_) => {}
                Expr::Compare { left, right, .. }
                | Expr::And(left, right)
                | Expr::Or(left, right) => {
                    stack.push(left);
                    stack.push(right);
                }
                Expr::Not(operand) | Expr::IsNull { operand, .. } => stack.push(operand),
            }
        }
    }
    needed
}

/// Whether any row of `stripe` could satisfy every conjunct.
///
/// Answers `true` whenever it is not certain, which is what makes pruning safe: a conservative
/// pruner reads a stripe it did not have to, and a wrong one loses a row.
fn stripe_can_match(
    stripe: &StripeMeta,
    conjuncts: &[&Expr],
    fragment: &Fragment,
    slots: &[ColumnType],
) -> bool {
    conjuncts.iter().all(|conjunct| {
        let Some((slot, test)) = predicate(conjunct) else {
            return true;
        };
        let Some(column) = fragment.projection.get(slot as usize) else {
            return true;
        };
        let (Some(stats), Some(ty)) = (
            stripe
                .columns
                .get(*column as usize)
                .map(|chunk| &chunk.stats),
            slots.get(slot as usize).copied(),
        ) else {
            return true;
        };
        can_match(stats, stripe.rows, ty, &test)
    })
}

/// A conjunct reduced to something the statistics can answer.
enum Test {
    /// `column op literal`, with the operator already oriented that way round.
    Compare(CompareOp, Value),
    /// `IS NULL`, or `IS NOT NULL`.
    Null { negated: bool },
}

/// Recognises the conjunct shapes a pruner can use, orienting the comparison so the column is on
/// the left. Anything else yields `None`, which reads the stripe.
fn predicate(expr: &Expr) -> Option<(u32, Test)> {
    match expr {
        Expr::Compare { op, left, right } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column(slot), Expr::Literal(value)) => {
                Some((*slot, Test::Compare(*op, value.clone())))
            }
            (Expr::Literal(value), Expr::Column(slot)) => {
                Some((*slot, Test::Compare(flip(*op), value.clone())))
            }
            _ => None,
        },
        Expr::IsNull { operand, negated } => match operand.as_ref() {
            Expr::Column(slot) => Some((*slot, Test::Null { negated: *negated })),
            _ => None,
        },
        _ => None,
    }
}

/// The same comparison with its operands swapped.
fn flip(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Eq => CompareOp::Eq,
        CompareOp::NotEq => CompareOp::NotEq,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::LtEq => CompareOp::GtEq,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::GtEq => CompareOp::LtEq,
    }
}

/// Whether a chunk with these statistics could hold a row satisfying `test`.
fn can_match(stats: &ColumnStats, rows: u64, ty: ColumnType, test: &Test) -> bool {
    match test {
        Test::Null { negated: false } => stats.null_count > 0,
        Test::Null { negated: true } => stats.null_count < rows,

        Test::Compare(op, literal) => {
            // A comparison with NULL is unknown for every row, so nothing can match it.
            if literal.is_null() {
                return false;
            }
            // Every row NULL: no comparison is ever true.
            if stats.null_count >= rows {
                return false;
            }
            let (Some(min), Some(max)) = (
                stats.min.as_ref().and_then(|bound| bound.as_value(ty)),
                stats.max.as_ref().and_then(|bound| bound.as_value(ty)),
            ) else {
                // No bounds recorded: nothing is known, so the stripe is read.
                return true;
            };

            match op {
                CompareOp::Eq => literal.pg_cmp(&min).is_ge() && literal.pg_cmp(&max).is_le(),
                // Some value is below the literal only if the smallest one is.
                CompareOp::Lt => min.pg_cmp(literal).is_lt(),
                CompareOp::LtEq => min.pg_cmp(literal).is_le(),
                CompareOp::Gt => max.pg_cmp(literal).is_gt(),
                CompareOp::GtEq => max.pg_cmp(literal).is_ge(),
                // Only a chunk in which every value equals the literal can be skipped, and a
                // widened bound cannot establish that — hence the exactness check.
                CompareOp::NotEq => {
                    !(stats.bounds_are_exact()
                        && min.pg_cmp(&max).is_eq()
                        && min.pg_cmp(literal).is_eq())
                }
            }
        }
    }
}
