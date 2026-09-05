//! A plan fragment: the unit of work a columnar node is asked to do.
//!
//! [ADR 0022](../../../../docs/adr/0022-columnar-learner-replica.md) decision 3 puts push-down on
//! the seam `TxnKvReq::Scan` already establishes — a request that carries *work* rather than a
//! range, and returns what the work produced rather than what it read. A fragment is that request:
//! a table, a key range, a projection, a filter over the projected columns, and a set of
//! aggregates with an optional grouping. "Scan, filter, project, aggregate", which is the query
//! shape the whole ADR exists for and which composes into a two-level aggregate rather than a
//! distributed one.
//!
//! # Refuse, never partially honour
//!
//! The rule this module exists to enforce. A fragment carrying anything this build does not
//! implement comes back as [`crate::Error::Refused`] with **none** of it done, and the caller
//! falls back to a row scan. Honouring the half it understood would silently drop a filter, which
//! returns extra rows rather than an error — the defect class `esker_sql::plan`'s "reject, do not
//! ignore" rule exists to make impossible, arrived at here independently and for the same reason.
//!
//! [`Fragment::validate`] is where every refusal is decided, against the schema of the file about
//! to be scanned, **before** a byte is read. That placement is the point: a fragment refused
//! half-way through a scan has already done work whose partial results somebody might use.
//!
//! # Slots
//!
//! The projection is the only place a table column index appears. Filters, groupings and
//! aggregates name **projection slots**, so an expression cannot reach a column the fragment did
//! not ask for, and "decode only what was projected" is a property of the format rather than a
//! discipline the evaluator keeps.

pub mod codec;
pub mod expr;

pub use codec::{FRAGMENT_FORMAT_VERSION, decode, encode};
pub use expr::{CompareOp, Expr};

use crate::error::{Error, Result};
use crate::value::{ColumnType, Schema};

/// How deeply a filter expression may nest.
///
/// A bound on the decoder's recursion before it is a bound on the language: without one, a
/// message of a few hundred bytes is a stack overflow, which is a panic on untrusted input by
/// another name (invariant 9). Thirty-two is far past any filter a planner writes.
pub const MAX_EXPR_DEPTH: usize = 32;

/// Most aggregates one fragment may ask for.
pub const MAX_AGGREGATES: usize = 256;

/// Most grouping columns one fragment may ask for.
pub const MAX_GROUP_BY: usize = 64;

/// Most values an [`expr::Expr::In`] list may carry.
///
/// A bound because the list arrives on the wire and every bound here is one a hostile or broken
/// peer cannot exceed. 4,096 is chosen against two things and neither is a guess: an `int8` list
/// that long encodes to well under a tenth of `max_frame_size` (16 MiB, `docs/DESIGN.md` §9), and
/// a binary search over it is twelve comparisons a row, against the four thousand an `Or` chain
/// would cost. What moves it is a measurement, not a preference.
pub const MAX_IN_VALUES: usize = 4_096;

/// Longest key-range bound a fragment may carry.
pub const MAX_KEY_BOUND: usize = 64 * 1024;

/// Which table a fragment reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableRef {
    /// The tenant that owns it.
    pub tenant: u64,
    /// The relation id, from the tenant's own sequence.
    pub table_id: u64,
}

/// The half-open key range a fragment restricts itself to.
///
/// An empty bound is unbounded on that side, which is how a whole-table fragment is written. This
/// build honours only the fully unbounded range and [refuses](Fragment::validate) any other: a
/// columnar file records no key range, so restricting to one is something it *cannot* do, and
/// carrying the field while ignoring it is exactly the defect the refuse rule exists for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyRange {
    /// Inclusive lower bound, or empty for unbounded.
    pub start: Vec<u8>,
    /// Exclusive upper bound, or empty for unbounded.
    pub end: Vec<u8>,
}

impl KeyRange {
    /// The whole table.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::default()
    }

    /// Whether this restricts nothing.
    #[must_use]
    pub fn is_unbounded(&self) -> bool {
        self.start.is_empty() && self.end.is_empty()
    }
}

/// One aggregate over a projection slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// `count(*)`: every row, including one whose every column is NULL.
    CountStar,
    /// `count(col)`: rows where the slot is not NULL.
    Count(u32),
    /// `sum(col)`: `int8` and `double` only. NULL over no rows.
    Sum(u32),
    /// `min(col)`, in [`crate::ValueRef::pg_cmp`] order. NULL over no rows.
    Min(u32),
    /// `max(col)`, likewise.
    Max(u32),
}

impl Aggregate {
    /// The tag byte this aggregate is stored as. Frozen.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Aggregate::CountStar => 1,
            Aggregate::Count(_) => 2,
            Aggregate::Sum(_) => 3,
            Aggregate::Min(_) => 4,
            Aggregate::Max(_) => 5,
        }
    }

    /// The slot this aggregate reads, or `None` for `count(*)`, which reads none.
    #[must_use]
    pub fn slot(self) -> Option<u32> {
        match self {
            Aggregate::CountStar => None,
            Aggregate::Count(slot)
            | Aggregate::Sum(slot)
            | Aggregate::Min(slot)
            | Aggregate::Max(slot) => Some(slot),
        }
    }

    /// What it is called, for messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Aggregate::CountStar | Aggregate::Count(_) => "count",
            Aggregate::Sum(_) => "sum",
            Aggregate::Min(_) => "min",
            Aggregate::Max(_) => "max",
        }
    }
}

/// What a fragment asks to be given back.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// The projected columns of the rows that matched, in row order.
    Rows {
        /// At most this many rows, or every one of them.
        ///
        /// A bound on work rather than an operator: with no ordering there is no "first" row to
        /// speak of, so this is what stops a fragment reading a whole table to answer a question
        /// about part of it, and nothing more.
        limit: Option<u64>,
    },
    /// Partial aggregates, one set per group.
    Aggregates {
        /// Projection slots to group by. Empty means one group over everything, which is what a
        /// bare `SELECT count(*) FROM t` is.
        group_by: Vec<u32>,
        /// The aggregates, in the order the answer reports them.
        aggregates: Vec<Aggregate>,
    },
}

/// A piece of a plan, evaluated where the data is.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    /// The table it reads.
    pub table: TableRef,
    /// The key range it restricts itself to.
    pub range: KeyRange,
    /// Table column indexes, in the order slots refer to them.
    pub projection: Vec<u32>,
    /// The `WHERE` clause, over projection slots.
    pub filter: Option<Expr>,
    /// What it returns.
    pub output: Output,
}

impl Fragment {
    /// A fragment that scans a whole table and returns the projected columns.
    #[must_use]
    pub fn scan(table: TableRef, projection: Vec<u32>) -> Self {
        Self {
            table,
            range: KeyRange::unbounded(),
            projection,
            filter: None,
            output: Output::Rows { limit: None },
        }
    }

    /// The same, aggregated.
    #[must_use]
    pub fn aggregate(
        table: TableRef,
        projection: Vec<u32>,
        group_by: Vec<u32>,
        aggregates: Vec<Aggregate>,
    ) -> Self {
        Self {
            table,
            range: KeyRange::unbounded(),
            projection,
            filter: None,
            output: Output::Aggregates {
                group_by,
                aggregates,
            },
        }
    }

    /// The type of every projection slot, given the file's schema.
    pub fn slot_types(&self, schema: &Schema) -> Result<Vec<ColumnType>> {
        self.projection
            .iter()
            .map(|column| {
                schema
                    .columns()
                    .get(*column as usize)
                    .map(|def| def.ty)
                    .ok_or_else(|| {
                        Error::refused(format!(
                            "column {column} of a file that has {}",
                            schema.len()
                        ))
                    })
            })
            .collect()
    }

    /// Decides every refusal, against the schema of the file about to be read.
    ///
    /// Everything this build cannot do is found here, before a byte is read, so that a refusal is
    /// always a refusal of the *whole* fragment. Returns the type of each projection slot on
    /// success, which is what the evaluator needs next anyway.
    pub fn validate(&self, schema: &Schema) -> Result<Vec<ColumnType>> {
        if !self.range.is_unbounded() {
            return Err(Error::refused(
                "a key range: a columnar file records none, so this build cannot restrict to one",
            ));
        }
        if self.projection.len() > crate::value::MAX_COLUMNS {
            return Err(Error::refused(format!(
                "a projection of {} columns",
                self.projection.len()
            )));
        }
        let slots = self.slot_types(schema)?;

        if let Some(filter) = &self.filter {
            // Depth first, and by the iterative measure: a recursive type check on an expression
            // deep enough to matter is the problem it is checking for.
            if filter.depth() > MAX_EXPR_DEPTH {
                return Err(Error::refused(format!(
                    "a filter nested {} deep, over the {MAX_EXPR_DEPTH} this build evaluates",
                    filter.depth()
                )));
            }
            let ty = check(filter, &slots)?;
            if ty != Some(ColumnType::Bool) {
                return Err(Error::refused(format!(
                    "a filter of type {}, which is not a condition",
                    ty.map_or("unknown", ColumnType::name)
                )));
            }
        }

        match &self.output {
            Output::Rows { .. } => {}
            Output::Aggregates {
                group_by,
                aggregates,
            } => {
                if group_by.len() > MAX_GROUP_BY {
                    return Err(Error::refused(format!(
                        "a grouping over {} columns",
                        group_by.len()
                    )));
                }
                if aggregates.len() > MAX_AGGREGATES {
                    return Err(Error::refused(format!("{} aggregates", aggregates.len())));
                }
                for slot in group_by {
                    slot_type(*slot, &slots)?;
                }
                for aggregate in aggregates {
                    let Some(slot) = aggregate.slot() else {
                        continue;
                    };
                    let ty = slot_type(slot, &slots)?;
                    if matches!(aggregate, Aggregate::Sum(_))
                        && !matches!(ty, ColumnType::Int8 | ColumnType::Double)
                    {
                        return Err(Error::refused(format!(
                            "sum over {}, which has no addition here",
                            ty.name()
                        )));
                    }
                }
            }
        }
        Ok(slots)
    }
}

/// The type of one slot, or a refusal naming the projection's width.
fn slot_type(slot: u32, slots: &[ColumnType]) -> Result<ColumnType> {
    slots.get(slot as usize).copied().ok_or_else(|| {
        Error::refused(format!(
            "slot {slot} of a projection with {} columns",
            slots.len()
        ))
    })
}

/// Type-checks an expression, returning what it produces.
///
/// Recursive, which is safe because [`Fragment::validate`] has already bounded the depth by
/// [`MAX_EXPR_DEPTH`] using an iterative measure.
fn check(expr: &Expr, slots: &[ColumnType]) -> Result<Option<ColumnType>> {
    Ok(match expr {
        Expr::Column(slot) => Some(slot_type(*slot, slots)?),
        Expr::Literal(value) => value.column_type(),

        Expr::Compare { op, left, right } => {
            let (left, right) = (check(left, slots)?, check(right, slots)?);
            // An untyped NULL takes the other side's type; two different types have no operator,
            // which is what a real server answers with `operator does not exist: text = integer`
            // rather than a silent `false`.
            if let (Some(left), Some(right)) = (left, right)
                && left != right
            {
                return Err(Error::refused(format!(
                    "operator does not exist: {} {} {}",
                    left.name(),
                    op.symbol(),
                    right.name()
                )));
            }
            Some(ColumnType::Bool)
        }

        // **The same type rule the comparison has, for the same reason.** Two implementations of
        // this system's ordering agree about values of one type by construction and about values
        // of two only by luck (ADR 0040 Decision 5), so a list whose values are not all the
        // operand's type is refused rather than compared.
        Expr::In { operand, values } => {
            let operand = check(operand, slots)?;
            for value in values {
                let Some(value_type) = value.column_type() else {
                    // A NULL in the list; the decoder refuses these, and a hand-built expression
                    // that has one is refused here rather than evaluated.
                    return Err(Error::refused("a NULL in an IN list".to_owned()));
                };
                if let Some(operand) = operand
                    && operand != value_type
                {
                    return Err(Error::refused(format!(
                        "operator does not exist: {} = {}",
                        operand.name(),
                        value_type.name()
                    )));
                }
            }
            Some(ColumnType::Bool)
        }

        Expr::And(left, right) | Expr::Or(left, right) => {
            for operand in [left, right] {
                let ty = check(operand, slots)?;
                if !matches!(ty, Some(ColumnType::Bool) | None) {
                    return Err(Error::refused(format!(
                        "argument of AND/OR must be type boolean, not {}",
                        ty.map_or("unknown", ColumnType::name)
                    )));
                }
            }
            Some(ColumnType::Bool)
        }

        Expr::Not(operand) => {
            let ty = check(operand, slots)?;
            if !matches!(ty, Some(ColumnType::Bool) | None) {
                return Err(Error::refused(format!(
                    "argument of NOT must be type boolean, not {}",
                    ty.map_or("unknown", ColumnType::name)
                )));
            }
            Some(ColumnType::Bool)
        }

        Expr::IsNull { operand, .. } => {
            check(operand, slots)?;
            Some(ColumnType::Bool)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{Aggregate, CompareOp, Expr, Fragment, KeyRange, MAX_EXPR_DEPTH, Output, TableRef};
    use crate::value::{ColumnDef, ColumnType, Schema, Value};

    fn schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("amount", ColumnType::Double),
            ColumnDef::new("live", ColumnType::Bool),
        ])
        .unwrap()
    }

    fn table() -> TableRef {
        TableRef {
            tenant: 1,
            table_id: 7,
        }
    }

    #[test]
    fn a_well_formed_fragment_validates_and_reports_its_slot_types() {
        let mut fragment = Fragment::scan(table(), vec![2, 0]);
        fragment.filter = Some(Expr::And(
            Box::new(Expr::compare(1, CompareOp::Gt, Value::Int8(10))),
            Box::new(Expr::IsNull {
                operand: Box::new(Expr::Column(0)),
                negated: true,
            }),
        ));
        assert_eq!(
            fragment.validate(&schema()).unwrap(),
            vec![ColumnType::Double, ColumnType::Int8]
        );
    }

    /// A key range is carried by the format and cannot be honoured by this build, so it is
    /// refused rather than ignored. This is the refuse rule's clearest case.
    #[test]
    fn a_bounded_key_range_is_refused_rather_than_ignored() {
        let mut fragment = Fragment::scan(table(), vec![0]);
        fragment.range = KeyRange {
            start: b"a".to_vec(),
            end: Vec::new(),
        };
        let error = fragment.validate(&schema()).unwrap_err();
        assert!(error.is_refused(), "{error}");
        assert!(error.to_string().contains("key range"), "{error}");

        fragment.range = KeyRange::unbounded();
        assert!(fragment.validate(&schema()).is_ok());
    }

    #[test]
    fn a_projection_outside_the_schema_is_refused() {
        let error = Fragment::scan(table(), vec![9])
            .validate(&schema())
            .unwrap_err();
        assert!(error.is_refused(), "{error}");
        assert!(error.to_string().contains("column 9"), "{error}");
    }

    #[test]
    fn a_slot_outside_the_projection_is_refused() {
        let mut fragment = Fragment::scan(table(), vec![0]);
        fragment.filter = Some(Expr::compare(3, CompareOp::Eq, Value::Int8(1)));
        let error = fragment.validate(&schema()).unwrap_err();
        assert!(error.to_string().contains("slot 3"), "{error}");

        let fragment = Fragment::aggregate(table(), vec![0], vec![4], vec![Aggregate::CountStar]);
        assert!(fragment.validate(&schema()).unwrap_err().is_refused());

        let fragment = Fragment::aggregate(table(), vec![0], Vec::new(), vec![Aggregate::Sum(2)]);
        assert!(fragment.validate(&schema()).unwrap_err().is_refused());
    }

    /// The row side answers `operator does not exist: text = integer` rather than a silent false,
    /// and so does this.
    #[test]
    fn comparing_two_different_types_is_refused() {
        let mut fragment = Fragment::scan(table(), vec![1]);
        fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Int8(1)));
        let error = fragment.validate(&schema()).unwrap_err();
        assert!(
            error.to_string().contains("operator does not exist"),
            "{error}"
        );

        // An untyped NULL takes whatever the other side is, so it is always comparable.
        fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Null));
        assert!(fragment.validate(&schema()).is_ok());

        // int8 and timestamptz are different types even though they are the same 64 bits.
        let stamped = Schema::new(vec![ColumnDef::new("at", ColumnType::TimestampTz)]).unwrap();
        let mut fragment = Fragment::scan(table(), vec![0]);
        fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Int8(1)));
        assert!(fragment.validate(&stamped).unwrap_err().is_refused());
    }

    #[test]
    fn a_filter_that_is_not_a_condition_is_refused() {
        let mut fragment = Fragment::scan(table(), vec![0]);
        fragment.filter = Some(Expr::Column(0));
        let error = fragment.validate(&schema()).unwrap_err();
        assert!(error.to_string().contains("not a condition"), "{error}");

        // A boolean column, though, is a perfectly good filter on its own.
        let mut fragment = Fragment::scan(table(), vec![3]);
        fragment.filter = Some(Expr::Column(0));
        assert!(fragment.validate(&schema()).is_ok());

        // And a non-boolean under a connective is refused by name.
        let mut fragment = Fragment::scan(table(), vec![0, 3]);
        fragment.filter = Some(Expr::And(
            Box::new(Expr::Column(0)),
            Box::new(Expr::Column(1)),
        ));
        assert!(
            fragment
                .validate(&schema())
                .unwrap_err()
                .to_string()
                .contains("must be type boolean")
        );
    }

    #[test]
    fn sum_exists_only_where_addition_does() {
        for (column, ok) in [(0u32, true), (2, true), (1, false), (3, false)] {
            let fragment =
                Fragment::aggregate(table(), vec![column], Vec::new(), vec![Aggregate::Sum(0)]);
            assert_eq!(fragment.validate(&schema()).is_ok(), ok, "column {column}");
        }
        // count, min and max work on anything.
        for aggregate in [Aggregate::Count(0), Aggregate::Min(0), Aggregate::Max(0)] {
            let fragment = Fragment::aggregate(table(), vec![1], Vec::new(), vec![aggregate]);
            assert!(fragment.validate(&schema()).is_ok(), "{aggregate:?}");
        }
    }

    #[test]
    fn a_filter_nested_past_the_limit_is_refused() {
        let mut deep = Expr::compare(0, CompareOp::Eq, Value::Int8(1));
        for _ in 0..MAX_EXPR_DEPTH {
            deep = Expr::Not(Box::new(deep));
        }
        let mut fragment = Fragment::scan(table(), vec![0]);
        fragment.filter = Some(deep);
        let error = fragment.validate(&schema()).unwrap_err();
        assert!(error.to_string().contains("nested"), "{error}");
    }

    /// `count(*)` needs no columns at all, which is worth allowing: the answer comes out of the
    /// footer and no chunk is read.
    #[test]
    fn a_fragment_may_project_nothing() {
        let fragment =
            Fragment::aggregate(table(), Vec::new(), Vec::new(), vec![Aggregate::CountStar]);
        assert_eq!(fragment.validate(&schema()).unwrap(), Vec::new());
    }

    #[test]
    fn aggregate_tags_and_slots_are_frozen() {
        assert_eq!(Aggregate::CountStar.as_u8(), 1);
        assert_eq!(Aggregate::Count(0).as_u8(), 2);
        assert_eq!(Aggregate::Sum(0).as_u8(), 3);
        assert_eq!(Aggregate::Min(0).as_u8(), 4);
        assert_eq!(Aggregate::Max(0).as_u8(), 5);
        assert_eq!(Aggregate::CountStar.slot(), None);
        assert_eq!(Aggregate::Max(7).slot(), Some(7));
        assert_eq!(Aggregate::CountStar.name(), "count");
        assert_eq!(Aggregate::Sum(0).name(), "sum");
    }

    #[test]
    fn an_unbounded_range_is_the_default() {
        assert!(KeyRange::unbounded().is_unbounded());
        assert!(KeyRange::default().is_unbounded());
        assert!(
            !KeyRange {
                start: Vec::new(),
                end: b"z".to_vec()
            }
            .is_unbounded()
        );
        assert!(matches!(
            Fragment::scan(table(), vec![0]).output,
            Output::Rows { limit: None }
        ));
    }
}
