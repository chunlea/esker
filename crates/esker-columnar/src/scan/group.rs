//! Grouping and accumulation: how a fragment's rows become partial aggregates.
//!
//! # A group is an equivalence class of [`crate::ValueRef::pg_cmp`]
//!
//! Which is PostgreSQL's `GROUP BY`, and it has two consequences worth stating because a naive
//! implementation gets both wrong. **NULL forms a group of its own** rather than disappearing —
//! `pg_cmp` makes NULL equal only to NULL, which is exactly the behaviour `GROUP BY` wants and
//! not the behaviour `=` has. And `-0.0` and `NaN` group by *value*, so `-0.0` and `0.0` land in
//! one group and every `NaN` lands in another, however their bits differ.
//!
//! Groups come back in `pg_cmp` order of their keys, and a group's key is the **first** one seen
//! for it. Both are determinism rather than taste: a harness that compares two implementations
//! byte for byte cannot do so if either is free to choose an order or a representative.
//!
//! # Accumulation order is part of the answer
//!
//! Floating-point addition is not associative, so `sum(double)` depends on the order the values
//! are added and on where the additions are grouped. This module therefore keeps **one
//! accumulator per group across the whole file**, folding left in row order — a fragment's answer
//! must not depend on where stripe boundaries happened to fall. [`Partial::combine`] exists for
//! the level above, where a SQL node finishes many fragments, and combining there *does* change
//! the answer; that is milestone 4's problem to state and this note is where it was first seen.

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::fragment::Aggregate;
use crate::value::{ColumnType, Value, ValueRef};

/// One group's key: the values of the grouping slots, compared as PostgreSQL groups them.
#[derive(Debug, Clone)]
pub struct GroupKey(Vec<Value>);

impl GroupKey {
    /// A key from the grouping slots' values.
    #[must_use]
    pub fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    /// The values, in the order the fragment named its grouping slots.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.0
    }

    /// Consumes the key for its values.
    #[must_use]
    pub fn into_values(self) -> Vec<Value> {
        self.0
    }
}

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

/// Reflexive because [`crate::ValueRef::pg_cmp`] makes every value equal to itself, `NaN`
/// included — which is the whole reason a `NaN` can be a group key at all.
impl Eq for GroupKey {}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        for (left, right) in self.0.iter().zip(&other.0) {
            let ordering = left.pg_cmp(right);
            if !ordering.is_eq() {
                return ordering;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

/// One aggregate's answer over the rows a fragment matched.
///
/// A *partial*: it is the whole answer for the file it was computed from, and combinable with the
/// partials of other files through [`Partial::combine`]. Nothing in this milestone combines any;
/// the operation exists so that its arithmetic — and its overflow — is settled here rather than
/// invented later by whoever first needs it.
#[derive(Debug, Clone, PartialEq)]
pub enum Partial {
    /// `count(*)` or `count(col)`. Combined by addition.
    Count(u64),
    /// `sum(col)`, NULL over no rows. Combined by addition, which can overflow.
    Sum(Option<Value>),
    /// `min(col)`, NULL over no rows.
    Min(Option<Value>),
    /// `max(col)`, NULL over no rows.
    Max(Option<Value>),
}

impl Partial {
    /// Folds `other` into this partial, as a SQL node would finishing a two-level aggregate.
    ///
    /// **Not associative for doubles.** Combining partials adds numbers that a single-level fold
    /// would have added in a different order, so a `sum(double)` finished from many fragments may
    /// differ in its last bits from the same query run over one. That is inherent to two-level
    /// aggregation rather than a defect here, and it is why a fragment's own answer is folded in
    /// row order across every stripe rather than per stripe and combined.
    pub fn combine(&mut self, other: &Partial) -> Result<()> {
        match (self, other) {
            (Partial::Count(left), Partial::Count(right)) => {
                *left = left
                    .checked_add(*right)
                    .ok_or_else(|| Error::Overflow(format!("count of {left} + {right} rows")))?;
            }
            (Partial::Sum(left), Partial::Sum(right)) => match (left.as_ref(), right) {
                (_, None) => {}
                (None, Some(right)) => *left = Some(right.clone()),
                (Some(a), Some(b)) => *left = Some(add(a, b)?),
            },
            (Partial::Min(left), Partial::Min(right)) => keep(left, right.as_ref(), Ordering::Less),
            (Partial::Max(left), Partial::Max(right)) => {
                keep(left, right.as_ref(), Ordering::Greater);
            }
            (left, right) => {
                return Err(Error::InvalidArgument(format!(
                    "combining {left:?} with {right:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Keeps whichever of the two extends the range in `wanted`, treating absence as no information.
fn keep(left: &mut Option<Value>, right: Option<&Value>, wanted: Ordering) {
    let Some(right) = right else { return };
    match left.as_ref() {
        None => *left = Some(right.clone()),
        Some(current) if right.pg_cmp(current) == wanted => *left = Some(right.clone()),
        Some(_) => {}
    }
}

/// Adds two sums of the same type.
///
/// Integer overflow is an error rather than a wrap. PostgreSQL's `sum(bigint)` returns `numeric`
/// and cannot overflow at all; phase 6a has no `numeric`, so the honest answer is to say the total
/// does not fit — a wrong total is worse than a missing one.
fn add(left: &Value, right: &Value) -> Result<Value> {
    Ok(match (left, right) {
        (Value::Int8(a), Value::Int8(b)) => Value::Int8(
            a.checked_add(*b)
                .ok_or_else(|| Error::Overflow(format!("bigint out of range: {a} + {b}")))?,
        ),
        (Value::Double(a), Value::Double(b)) => Value::Double(a + b),
        (a, b) => {
            return Err(Error::InvalidArgument(format!("summing {a:?} with {b:?}")));
        }
    })
}

/// One aggregate mid-flight.
#[derive(Debug, Clone)]
pub(crate) enum Accumulator {
    /// `count(*)`: every row.
    CountStar(u64),
    /// `count(col)`: rows whose slot is not NULL.
    Count { slot: usize, count: u64 },
    /// `sum(col)`, NULL until a row contributes.
    Sum {
        slot: usize,
        total: Option<Value>,
        /// Whether the column is an integer, which decides whether adding can overflow.
        integral: bool,
    },
    /// `min(col)` or `max(col)`.
    Extreme {
        slot: usize,
        /// The ordering that wins: `Less` for a minimum, `Greater` for a maximum.
        wanted: Ordering,
        best: Option<Value>,
        /// The column's type, for turning a reference back into a value.
        ty: ColumnType,
    },
}

impl Accumulator {
    /// A fresh accumulator for `aggregate`, given the types of the projection's slots.
    pub(crate) fn new(aggregate: Aggregate, slots: &[ColumnType]) -> Self {
        let ty = |slot: u32| slots[slot as usize];
        match aggregate {
            Aggregate::CountStar => Accumulator::CountStar(0),
            Aggregate::Count(slot) => Accumulator::Count {
                slot: slot as usize,
                count: 0,
            },
            Aggregate::Sum(slot) => Accumulator::Sum {
                slot: slot as usize,
                total: None,
                integral: ty(slot) == ColumnType::Int8,
            },
            Aggregate::Min(slot) => Accumulator::Extreme {
                slot: slot as usize,
                wanted: Ordering::Less,
                best: None,
                ty: ty(slot),
            },
            Aggregate::Max(slot) => Accumulator::Extreme {
                slot: slot as usize,
                wanted: Ordering::Greater,
                best: None,
                ty: ty(slot),
            },
        }
    }

    /// Folds one matched row in.
    pub(crate) fn push(&mut self, row: &[ValueRef<'_>]) -> Result<()> {
        let at = |slot: usize| row.get(slot).copied().unwrap_or(ValueRef::Null);
        match self {
            Accumulator::CountStar(count) => *count += 1,
            Accumulator::Count { slot, count } => {
                if !at(*slot).is_null() {
                    *count += 1;
                }
            }
            Accumulator::Sum {
                slot,
                total,
                integral,
            } => {
                let value = at(*slot);
                if value.is_null() {
                    return Ok(());
                }
                let addend = match (value, *integral) {
                    (ValueRef::Int(v), true) => Value::Int8(v),
                    (ValueRef::Double(v), false) => Value::Double(v),
                    (other, _) => {
                        return Err(Error::InvalidArgument(format!("summing {other:?}")));
                    }
                };
                *total = Some(match total.take() {
                    None => addend,
                    Some(current) => add(&current, &addend)?,
                });
            }
            Accumulator::Extreme {
                slot,
                wanted,
                best,
                ty,
            } => {
                let value = at(*slot);
                if value.is_null() {
                    return Ok(());
                }
                let value = value.to_value(*ty)?;
                if best
                    .as_ref()
                    .is_none_or(|current| value.pg_cmp(current) == *wanted)
                {
                    *best = Some(value);
                }
            }
        }
        Ok(())
    }

    /// The answer so far.
    pub(crate) fn finish(&self) -> Partial {
        match self {
            Accumulator::CountStar(count) | Accumulator::Count { count, .. } => {
                Partial::Count(*count)
            }
            Accumulator::Sum { total, .. } => Partial::Sum(total.clone()),
            Accumulator::Extreme {
                wanted: Ordering::Less,
                best,
                ..
            } => Partial::Min(best.clone()),
            Accumulator::Extreme { best, .. } => Partial::Max(best.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{Accumulator, GroupKey, Partial};
    use crate::fragment::Aggregate;
    use crate::value::{ColumnType, Value};

    #[test]
    fn a_group_key_orders_the_way_postgresql_groups() {
        let key = |values: Vec<Value>| GroupKey::new(values);

        // NULL is its own group, and it sorts last.
        assert_eq!(key(vec![Value::Null]), key(vec![Value::Null]));
        assert_ne!(key(vec![Value::Null]), key(vec![Value::Int8(0)]));
        assert!(key(vec![Value::Int8(0)]) < key(vec![Value::Null]));

        // -0.0 and 0.0 are one value, so they are one group.
        assert_eq!(
            key(vec![Value::Double(-0.0)]),
            key(vec![Value::Double(0.0)])
        );
        // And every NaN is one group, above every number.
        assert_eq!(
            key(vec![Value::Double(f64::NAN)]),
            key(vec![Value::Double(f64::from_bits(0xfff8_0000_dead_beef))])
        );
        assert!(key(vec![Value::Double(f64::INFINITY)]) < key(vec![Value::Double(f64::NAN)]));

        // Text by bytes, not by a collation.
        assert!(key(vec![Value::Text("B".into())]) < key(vec![Value::Text("a".into())]));

        let empty: Vec<Value> = Vec::new();
        assert_eq!(key(empty.clone()).values(), &[] as &[Value]);
        assert_eq!(
            key(vec![Value::Int8(1)]).into_values(),
            vec![Value::Int8(1)]
        );
    }

    fn fold(aggregate: Aggregate, ty: ColumnType, rows: &[Value]) -> Partial {
        let mut accumulator = Accumulator::new(aggregate, &[ty]);
        for value in rows {
            accumulator.push(&[value.as_ref()]).unwrap();
        }
        accumulator.finish()
    }

    /// The rules the row side never had, so they are defined here against PostgreSQL.
    #[test]
    fn the_aggregate_rules() {
        let rows = vec![Value::Int8(1), Value::Null, Value::Int8(2)];

        assert_eq!(
            fold(Aggregate::CountStar, ColumnType::Int8, &rows),
            Partial::Count(3),
            "count(*) counts rows, NULLs included"
        );
        assert_eq!(
            fold(Aggregate::Count(0), ColumnType::Int8, &rows),
            Partial::Count(2),
            "count(col) skips NULLs"
        );
        assert_eq!(
            fold(Aggregate::Sum(0), ColumnType::Int8, &rows),
            Partial::Sum(Some(Value::Int8(3)))
        );
        assert_eq!(
            fold(Aggregate::Min(0), ColumnType::Int8, &rows),
            Partial::Min(Some(Value::Int8(1)))
        );
        assert_eq!(
            fold(Aggregate::Max(0), ColumnType::Int8, &rows),
            Partial::Max(Some(Value::Int8(2)))
        );

        // Over no rows at all, and over nothing but NULLs: NULL, never zero.
        for rows in [Vec::new(), vec![Value::Null, Value::Null]] {
            assert_eq!(
                fold(Aggregate::CountStar, ColumnType::Int8, &rows),
                Partial::Count(rows.len() as u64)
            );
            assert_eq!(
                fold(Aggregate::Count(0), ColumnType::Int8, &rows),
                Partial::Count(0)
            );
            assert_eq!(
                fold(Aggregate::Sum(0), ColumnType::Int8, &rows),
                Partial::Sum(None),
                "a sum over nothing is NULL, not 0"
            );
            assert_eq!(
                fold(Aggregate::Min(0), ColumnType::Int8, &rows),
                Partial::Min(None)
            );
            assert_eq!(
                fold(Aggregate::Max(0), ColumnType::Int8, &rows),
                Partial::Max(None)
            );
        }
    }

    /// `NaN` is the largest value here, so it is the maximum and never the minimum.
    #[test]
    fn extremes_use_this_systems_order() {
        let rows = vec![
            Value::Double(1.0),
            Value::Double(f64::NAN),
            Value::Double(f64::INFINITY),
        ];
        let Partial::Max(Some(Value::Double(max))) =
            fold(Aggregate::Max(0), ColumnType::Double, &rows)
        else {
            panic!("max is not a double");
        };
        assert!(max.is_nan(), "NaN is the maximum");
        assert_eq!(
            fold(Aggregate::Min(0), ColumnType::Double, &rows),
            Partial::Min(Some(Value::Double(1.0)))
        );
    }

    /// A wrapped total is worse than a missing one, so it is an error on both sides.
    #[test]
    fn an_integer_sum_that_does_not_fit_is_an_error() {
        let mut accumulator = Accumulator::new(Aggregate::Sum(0), &[ColumnType::Int8]);
        accumulator.push(&[Value::Int8(i64::MAX).as_ref()]).unwrap();
        let error = accumulator.push(&[Value::Int8(1).as_ref()]).unwrap_err();
        assert!(error.is_overflow(), "{error}");
        assert!(error.to_string().contains("bigint out of range"), "{error}");

        // A double sum saturates to infinity instead, which is IEEE and not an error.
        let mut accumulator = Accumulator::new(Aggregate::Sum(0), &[ColumnType::Double]);
        for _ in 0..2 {
            accumulator
                .push(&[Value::Double(f64::MAX).as_ref()])
                .unwrap();
        }
        assert_eq!(
            accumulator.finish(),
            Partial::Sum(Some(Value::Double(f64::INFINITY)))
        );
    }

    #[test]
    fn partials_combine_the_way_a_second_level_would() {
        let mut count = Partial::Count(3);
        count.combine(&Partial::Count(4)).unwrap();
        assert_eq!(count, Partial::Count(7));

        let mut sum = Partial::Sum(Some(Value::Int8(10)));
        sum.combine(&Partial::Sum(None)).unwrap();
        assert_eq!(
            sum,
            Partial::Sum(Some(Value::Int8(10))),
            "NULL adds nothing"
        );
        sum.combine(&Partial::Sum(Some(Value::Int8(5)))).unwrap();
        assert_eq!(sum, Partial::Sum(Some(Value::Int8(15))));

        let mut empty = Partial::Sum(None);
        empty.combine(&Partial::Sum(Some(Value::Int8(2)))).unwrap();
        assert_eq!(empty, Partial::Sum(Some(Value::Int8(2))));

        let mut min = Partial::Min(Some(Value::Int8(4)));
        min.combine(&Partial::Min(Some(Value::Int8(9)))).unwrap();
        assert_eq!(min, Partial::Min(Some(Value::Int8(4))));
        min.combine(&Partial::Min(Some(Value::Int8(1)))).unwrap();
        assert_eq!(min, Partial::Min(Some(Value::Int8(1))));

        let mut max = Partial::Max(None);
        max.combine(&Partial::Max(Some(Value::Text("a".into()))))
            .unwrap();
        assert_eq!(max, Partial::Max(Some(Value::Text("a".into()))));

        // Overflow survives the second level too.
        let mut sum = Partial::Sum(Some(Value::Int8(i64::MAX)));
        assert!(
            sum.combine(&Partial::Sum(Some(Value::Int8(1))))
                .unwrap_err()
                .is_overflow()
        );
        // And two different aggregates cannot be combined at all.
        assert!(Partial::Count(1).combine(&Partial::Min(None)).is_err());
    }

    #[test]
    fn accumulators_report_the_shape_they_were_built_for() {
        let slots = [ColumnType::Double];
        assert!(matches!(
            Accumulator::new(Aggregate::Min(0), &slots).finish(),
            Partial::Min(None)
        ));
        assert!(matches!(
            Accumulator::new(Aggregate::Max(0), &slots).finish(),
            Partial::Max(None)
        ));
        let Accumulator::Extreme { wanted, .. } = Accumulator::new(Aggregate::Min(0), &slots)
        else {
            panic!("min is not an extreme");
        };
        assert_eq!(wanted, Ordering::Less);
    }
}
