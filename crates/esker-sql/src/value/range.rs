//! `daterange` — a half-open range of dates, as a value and as text.
//!
//! # Why text rather than a `Datum` variant
//!
//! A range reaches this node only as an **expression**: the suite's `EXCLUDE` constraint keys on
//! `daterange(start_date, end_date)` and no column is ever declared one. A `Datum` variant would
//! have to be threaded through `esker_keys`'s row codec and the columnar encoder for a value that
//! is never stored, which is the same trade `crate::value::vector` records for the catalog's
//! arrays. What it costs is the declared type: `pg_typeof` says `text` here and `daterange` there,
//! declared as a divergence.
//!
//! # The three facts a reader would get wrong
//!
//! * **It is half-open.** `[2026-01-01,2026-02-01)` and `[2026-02-01,2026-03-01)` do **not**
//!   overlap, which is exactly what lets the suite's adjacent rows both insert. Reading a range as
//!   closed refuses a row PostgreSQL admits.
//! * **A NULL bound is *unbounded*, not NULL.** `daterange(NULL, x)` prints `(,x)` and
//!   `daterange(NULL, NULL)` overlaps everything.
//! * **An empty range overlaps nothing, itself included.** A range whose start is not below its
//!   end is `empty`, and `&&` against it is always false.

use crate::value::{Datum, PgDatum};

/// A `daterange`, as days from 2000-01-01 — the representation [`Datum::Date`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    /// Inclusive lower bound, or `None` for unbounded below.
    pub start: Option<i32>,
    /// **Exclusive** upper bound, or `None` for unbounded above.
    pub end: Option<i32>,
    /// Whether the range is empty, which is not the same as having no bounds.
    pub empty: bool,
}

impl DateRange {
    /// `daterange(a, b)` — the constructor, with PostgreSQL's own emptiness rule.
    #[must_use]
    pub fn new(start: Option<i32>, end: Option<i32>) -> Self {
        // Empty when the bounds meet or cross: `[x,x)` contains nothing, and it is the case the
        // suite's `isempty` line pins.
        let empty = matches!((start, end), (Some(low), Some(high)) if low >= high);
        DateRange { start, end, empty }
    }

    /// Whether two ranges share any day.
    ///
    /// Half-open on both sides, so touching ranges do not overlap — `[a,b)` and `[b,c)` are
    /// disjoint. An empty range overlaps nothing.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        if self.empty || other.empty {
            return false;
        }
        // Unbounded ends never stop an overlap, which is what `None` means on each side.
        let after = match (self.start, other.end) {
            (Some(low), Some(high)) => low >= high,
            _ => false,
        };
        let before = match (self.end, other.start) {
            (Some(high), Some(low)) => high <= low,
            _ => false,
        };
        !after && !before
    }

    /// The text form: `[2026-01-01,2026-02-01)`, `(,2026-02-01)`, or `empty`.
    ///
    /// **An unbounded end prints as nothing between the delimiters and takes a round bracket**, so
    /// `daterange(NULL, x)` is `(,x)` and not `[,x)`. Measured.
    #[must_use]
    pub fn to_text(self) -> String {
        if self.empty {
            return "empty".to_owned();
        }
        let bound = |day: Option<i32>| day.map(crate::value::date::to_text).unwrap_or_default();
        format!(
            "{}{},{})",
            if self.start.is_some() { '[' } else { '(' },
            bound(self.start),
            bound(self.end)
        )
    }

    /// Reads one back from the text form.
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        if text == "empty" {
            return Some(DateRange {
                start: None,
                end: None,
                empty: true,
            });
        }
        let inner = text
            .strip_prefix(['[', '('])
            .and_then(|rest| rest.strip_suffix([')', ']']))?;
        let (low, high) = inner.split_once(',')?;
        let day = |text: &str| -> Option<Option<i32>> {
            if text.is_empty() {
                return Some(None);
            }
            match <Datum as PgDatum>::from_text(crate::value::ColumnType::Date, text) {
                Ok(Datum::Date(day)) => Some(Some(day)),
                _ => None,
            }
        };
        Some(DateRange::new(day(low)?, day(high)?))
    }
}

#[cfg(test)]
mod tests {
    use super::DateRange;

    /// **Half-open**: touching ranges are disjoint, which is what admits the suite's second row.
    #[test]
    fn touching_ranges_do_not_overlap() {
        let first = DateRange::new(Some(0), Some(31));
        let second = DateRange::new(Some(31), Some(59));
        assert!(!first.overlaps(second));
        assert!(!second.overlaps(first));
        assert!(first.overlaps(DateRange::new(Some(14), Some(45))));
    }

    /// An empty range overlaps nothing, **including itself**.
    #[test]
    fn an_empty_range_overlaps_nothing() {
        let empty = DateRange::new(Some(10), Some(10));
        assert!(empty.empty);
        assert!(!empty.overlaps(empty));
        assert!(!empty.overlaps(DateRange::new(Some(0), Some(31))));
        assert_eq!(empty.to_text(), "empty");
    }

    /// An unbounded end is not a NULL: it overlaps everything on that side.
    #[test]
    fn an_unbounded_end_overlaps_everything_beyond_it() {
        let all = DateRange::new(None, None);
        assert!(all.overlaps(DateRange::new(Some(0), Some(31))));
        let below = DateRange::new(None, Some(31));
        assert!(below.overlaps(DateRange::new(Some(0), Some(31))));
        assert!(!below.overlaps(DateRange::new(Some(31), Some(59))));
        // And the round bracket the missing bound takes.
        assert!(below.to_text().starts_with('('));
    }

    /// The text form round-trips.
    ///
    /// **An empty range keeps no bounds**, which is not a loss: `empty` is all PostgreSQL prints
    /// and `'empty'::daterange` has no bounds to read back either. So emptiness is compared rather
    /// than the fields, which is the only thing the text claims.
    #[test]
    fn the_text_form_round_trips() {
        for range in [
            DateRange::new(Some(0), Some(31)),
            DateRange::new(None, Some(31)),
            DateRange::new(Some(0), None),
            DateRange::new(None, None),
        ] {
            let text = range.to_text();
            assert_eq!(DateRange::from_text(&text), Some(range), "{text}");
        }
        let empty = DateRange::new(Some(5), Some(5));
        assert_eq!(empty.to_text(), "empty");
        assert!(DateRange::from_text("empty").is_some_and(|read| read.empty));
    }
}
