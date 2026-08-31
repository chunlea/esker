//! Comparing two fragment answers, with floating point compared by **bits**.
//!
//! Shared by the differential harness and the fuzz through `#[path]`, because both need exactly
//! the same notion of "the same answer" and a second one would eventually disagree with the
//! first. Test scaffolding rather than crate surface.
//!
//! `==` is wrong here in both directions: `NaN != NaN` would make two `NaN` answers agree without
//! being looked at, and `-0.0 == 0.0` would let a sign difference through. Both are in the corpus
//! on purpose.

#![allow(dead_code)]

use esker_columnar::{FragmentOutput, Partial, Value};

/// Whether two values are the same, floating point by bits.
pub(crate) fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        (a, b) => a == b,
    }
}

/// Whether two partial aggregates are the same, including which aggregate they are.
pub(crate) fn same_partial(left: &Partial, right: &Partial) -> bool {
    match (left, right) {
        (Partial::Count(a), Partial::Count(b)) => a == b,
        (Partial::Sum(a), Partial::Sum(b))
        | (Partial::Min(a), Partial::Min(b))
        | (Partial::Max(a), Partial::Max(b)) => match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => same_value(a, b),
            _ => false,
        },
        _ => false,
    }
}

/// Whether two fragment answers are the same, row for row and group for group.
pub(crate) fn same_output(left: &FragmentOutput, right: &FragmentOutput) -> bool {
    match (left, right) {
        (FragmentOutput::Rows(a), FragmentOutput::Rows(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(left, right)| {
                    left.len() == right.len()
                        && left.iter().zip(right).all(|(a, b)| same_value(a, b))
                })
        }
        (FragmentOutput::Groups(a), FragmentOutput::Groups(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(left, right)| {
                    left.key.len() == right.key.len()
                        && left
                            .key
                            .iter()
                            .zip(&right.key)
                            .all(|(a, b)| same_value(a, b))
                        && left.aggregates.len() == right.aggregates.len()
                        && left
                            .aggregates
                            .iter()
                            .zip(&right.aggregates)
                            .all(|(a, b)| same_partial(a, b))
                })
        }
        _ => false,
    }
}
