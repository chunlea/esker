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
pub mod merged;
pub mod visible;

use std::collections::BTreeMap;

use crate::column::Column;
use crate::error::{Error, Result};
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
/// No longer `Copy`: [`ScanOptions::visibility`] owns the key columns it names, and a scan's
/// switches are set once per scan rather than in a loop, so cloning them costs nothing worth
/// keeping a `Copy` bound for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Resolve MVCC versions while scanning, keeping the newest visible one per key.
    ///
    /// `None` scans every row in the file, which is what a run of one version per key wants and
    /// what M1 and M2 were written against. `Some` is a columnar learner answering a fragment at a
    /// read timestamp (ADR 0022 Decision 4) — see [`visible`] for why this is a mode rather than a
    /// filter, and why it **turns pruning off**.
    pub visibility: Option<visible::Visibility>,

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
        Self {
            prune: true,
            visibility: None,
        }
    }
}

/// Where matched rows go, and the only place a fragment's `output` is interpreted.
///
/// Shared by both scan paths so that the merged one cannot drift from the single-file one: a
/// filter, a projection or an accumulation written twice is two chances to be different, and the
/// difference would show up as two engines disagreeing, which ADR 0022 names as the worst failure
/// this feature can have.
struct Sink<'a> {
    fragment: &'a Fragment,
    slots: &'a [ColumnType],
    limit: Option<u64>,
    rows: Vec<Vec<Value>>,
    groups: BTreeMap<GroupKey, Vec<Accumulator>>,
}

impl<'a> Sink<'a> {
    fn new(fragment: &'a Fragment, slots: &'a [ColumnType]) -> Self {
        Self {
            fragment,
            slots,
            limit: match &fragment.output {
                Output::Rows { limit } => *limit,
                Output::Aggregates { .. } => None,
            },
            rows: Vec::new(),
            groups: BTreeMap::new(),
        }
    }

    /// Offers one row, already known visible. Returns `false` when the limit is reached.
    fn push(&mut self, row: &[ValueRef<'_>], stats: &mut ScanStats) -> Result<bool> {
        if let Some(filter) = &self.fragment.filter
            && !filter.matches(row)
        {
            return Ok(true);
        }
        stats.rows_matched += 1;
        match &self.fragment.output {
            Output::Rows { .. } => {
                self.rows.push(
                    row.iter()
                        .zip(self.slots)
                        .map(|(value, ty)| value.to_value(*ty))
                        .collect::<Result<Vec<_>>>()?,
                );
                Ok(self.limit.is_none_or(|limit| stats.rows_matched < limit))
            }
            Output::Aggregates {
                group_by,
                aggregates,
            } => {
                let key = GroupKey::new(
                    group_by
                        .iter()
                        .map(|slot| row[*slot as usize].to_value(self.slots[*slot as usize]))
                        .collect::<Result<Vec<_>>>()?,
                );
                let slots = self.slots;
                let accumulators = self.groups.entry(key).or_insert_with(|| {
                    aggregates
                        .iter()
                        .map(|aggregate| Accumulator::new(*aggregate, slots))
                        .collect()
                });
                for accumulator in accumulators.iter_mut() {
                    accumulator.push(row)?;
                }
                Ok(true)
            }
        }
    }

    fn finish(mut self) -> FragmentOutput {
        match &self.fragment.output {
            Output::Rows { .. } => FragmentOutput::Rows(self.rows),
            Output::Aggregates {
                group_by,
                aggregates,
            } => {
                // **An ungrouped aggregate over no rows is one group, not none.**
                // `SELECT count(*) FROM t WHERE false` is `0`, and a scan that returned no groups
                // would make it *no row at all*. A `GROUP BY` over no rows really is no groups,
                // which is why this is conditioned on there being no grouping slots.
                if group_by.is_empty() && self.groups.is_empty() {
                    self.groups.insert(
                        GroupKey::new(Vec::new()),
                        aggregates
                            .iter()
                            .map(|aggregate| Accumulator::new(*aggregate, self.slots))
                            .collect(),
                    );
                }
                FragmentOutput::Groups(
                    self.groups
                        .into_iter()
                        .map(|(key, accumulators)| Group {
                            key: key.into_values(),
                            aggregates: accumulators.iter().map(Accumulator::finish).collect(),
                        })
                        .collect::<Vec<_>>(),
                )
            }
        }
    }
}

/// Evaluates `fragment` against `reader`.
///
/// Every refusal happens in [`Fragment::validate`], before a byte is read, so a fragment that
/// gets past the first line here will be evaluated in full or fail on damaged data.
///
/// One file only. A region's columnar copy is several runs, and resolving MVCC visibility over
/// them one at a time is wrong — see [`evaluate_merged`].
pub fn evaluate(reader: &Reader, fragment: &Fragment) -> Result<FragmentResult> {
    evaluate_with(reader, fragment, &ScanOptions::default())
}

/// [`evaluate`], with the scan's switches exposed.
pub fn evaluate_with(
    reader: &Reader,
    fragment: &Fragment,
    options: &ScanOptions,
) -> Result<FragmentResult> {
    let slots = fragment.validate(reader.schema())?;
    let needed = needed_slots(fragment, slots.len());
    let mut conjuncts = Vec::new();
    if let Some(filter) = &fragment.filter {
        filter.conjuncts(&mut conjuncts);
    }

    let mut stats = ScanStats::default();
    // Carried across stripes: a key's versions may span them, so "already settled" is a fact about
    // the scan and not about one stripe.
    let mut resolver = visible::Resolver::default();
    let mut sink = Sink::new(fragment, &slots);

    for (index, stripe) in reader.stripes().iter().enumerate() {
        stats.stripes_considered += 1;
        if sink.limit.is_some_and(|limit| stats.rows_matched >= limit) {
            continue;
        }
        // **Pruning is unsound under visibility**, so `Visibility` overrides the switch rather
        // than trusting the caller to have turned it off. Pruning skips a stripe that cannot match
        // the *filter*, but visibility chooses the candidate row *before* the filter sees it, so a
        // pruned stripe can hide the version that should have won and let an overwritten one
        // through. `visible`'s header works the case through.
        if options.prune
            && options.visibility.is_none()
            && !stripe_can_match(stripe, &conjuncts, fragment, &slots)
        {
            continue;
        }
        stats.stripes_read += 1;

        let columns = decode_needed(reader, index, fragment, &needed)?;
        stats.chunks_decoded += columns.iter().filter(|column| column.is_some()).count() as u64;
        let mut cursors: Vec<Option<crate::column::ColumnIter<'_>>> = columns
            .iter()
            .map(|column| column.as_ref().map(Column::iter))
            .collect();

        let visibility_columns = match &options.visibility {
            Some(visibility) => Some(visibility.columns(reader, index)?),
            None => None,
        };
        let mut visibility_cursors: Vec<crate::column::ColumnIter<'_>> = visibility_columns
            .iter()
            .flatten()
            .map(Column::iter)
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
            if let Some(visibility) = &options.visibility
                && !next_is_visible(visibility, &mut visibility_cursors, &mut resolver)
            {
                continue;
            }
            if !sink.push(&row, &mut stats)? {
                break;
            }
        }
    }

    Ok(FragmentResult {
        output: sink.finish(),
        stats,
    })
}

/// [`evaluate_with`], over **every live run of a region at once**.
///
/// # Why this exists, and why per-run evaluation is wrong
///
/// MVCC visibility is a property of the **region**, not of a file. Resolving it per run answers
/// "the newest version of this key *in this run*", which is not the newest version — and the two
/// differ exactly when a key's history spans runs, which is the normal case for anything that has
/// been written to more than once.
///
/// Caught by the store's differential rather than reasoned out in advance: a key whose tombstone
/// landed in a later run kept returning its older, live-looking row, because the run holding the
/// tombstone resolves that key to *nothing* and so has nothing to say about it. There is no way to
/// repair that by combining per-run answers afterwards — for `Rows` you would have to carry each
/// candidate's timestamp and take the maximum, and **for aggregates it is impossible in
/// principle**, because a run cannot know its candidate was overruled by another run's.
///
/// So the runs are merged into one stream first. Each is already sorted `(key, commit_ts DESC)`,
/// so a k-way merge is globally sorted and the resolver works over it unchanged — the same thing
/// an LSM read does across its levels, for the same reason.
///
/// # The cost, stated rather than hidden
///
/// The merged path materialises each row as owned [`Value`]s, where the single-run path borrows
/// straight out of the decoded chunk. One run keeps the fast path; several pay for the merge. That
/// is measured in the phase's bench rather than asserted here.
pub fn evaluate_merged(
    readers: &[Reader],
    fragment: &Fragment,
    options: &ScanOptions,
) -> Result<FragmentResult> {
    match readers {
        [] => Err(Error::InvalidArgument("a region with no runs".into())),
        [only] => evaluate_with(only, fragment, options),
        _ => merged::evaluate(readers, fragment, options),
    }
}

/// Advances the version cursors one row and says whether that row is the visible one.
///
/// Split out of [`evaluate_with`] to keep it readable, and because "is this row visible" is a
/// question about the *run's* columns rather than about the fragment's slots — the two sets are
/// independent, and a fragment need not project a single column this reads.
fn next_is_visible(
    visibility: &visible::Visibility,
    cursors: &mut [crate::column::ColumnIter<'_>],
    resolver: &mut visible::Resolver,
) -> bool {
    let mut seen: Vec<ValueRef<'_>> = Vec::with_capacity(cursors.len());
    for cursor in cursors.iter_mut() {
        seen.push(cursor.next().unwrap_or(ValueRef::Null));
    }
    let keys = visibility.key_columns.len();
    let commit_ts = match seen.get(keys) {
        Some(ValueRef::Int(ts)) => *ts,
        // A run whose timestamp column is not an integer is one this build did not write.
        // Treating it as the beginning of time hides nothing: it can only ever be a candidate,
        // and the key still settles on it.
        _ => i64::MIN,
    };
    let deleted = matches!(seen.get(keys + 1), Some(ValueRef::Bool(true)));
    resolver.visible(&seen[..keys], commit_ts, deleted, visibility.ts)
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
