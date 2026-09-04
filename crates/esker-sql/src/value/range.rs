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

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum, PgDatum};

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
    pub fn new(start: Option<i32>, end: Option<i32>) -> Result<Self> {
        // **Meeting is empty; crossing is an error.** `[x,x)` contains nothing and is the case the
        // suite's `isempty` line pins; `[y,x)` with `y > x` is `22000` on a real server, and
        // answering `empty` for it was the worse of the two wrong answers this sweep found — a
        // query that filters nothing and reports nothing wrong.
        if matches!((start, end), (Some(low), Some(high)) if low > high) {
            return Err(SqlError::RangeBoundsOutOfOrder);
        }
        let empty = matches!((start, end), (Some(low), Some(high)) if low >= high);
        Ok(DateRange { start, end, empty })
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
            match <Datum as PgDatum>::from_text(ColumnType::Date, text) {
                Ok(Datum::Date(day)) => Some(Some(day)),
                _ => None,
            }
        };
        // A stored range came from a `new` that already checked its bounds, so a crossed pair
        // here is corruption rather than user input — `None` is this function's answer to
        // anything it cannot read, and that is what one is.
        DateRange::new(day(low)?, day(high)?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::DateRange;

    /// **Half-open**: touching ranges are disjoint, which is what admits the suite's second row.
    #[test]
    fn touching_ranges_do_not_overlap() {
        let first = DateRange::new(Some(0), Some(31)).unwrap();
        let second = DateRange::new(Some(31), Some(59)).unwrap();
        assert!(!first.overlaps(second));
        assert!(!second.overlaps(first));
        assert!(first.overlaps(DateRange::new(Some(14), Some(45)).unwrap()));
    }

    /// An empty range overlaps nothing, **including itself**.
    #[test]
    fn an_empty_range_overlaps_nothing() {
        let empty = DateRange::new(Some(10), Some(10)).unwrap();
        assert!(empty.empty);
        assert!(!empty.overlaps(empty));
        assert!(!empty.overlaps(DateRange::new(Some(0), Some(31)).unwrap()));
        assert_eq!(empty.to_text(), "empty");
    }

    /// An unbounded end is not a NULL: it overlaps everything on that side.
    #[test]
    fn an_unbounded_end_overlaps_everything_beyond_it() {
        let all = DateRange::new(None, None).unwrap();
        assert!(all.overlaps(DateRange::new(Some(0), Some(31)).unwrap()));
        let below = DateRange::new(None, Some(31)).unwrap();
        assert!(below.overlaps(DateRange::new(Some(0), Some(31)).unwrap()));
        assert!(!below.overlaps(DateRange::new(Some(31), Some(59)).unwrap()));
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
            DateRange::new(Some(0), Some(31)).unwrap(),
            DateRange::new(None, Some(31)).unwrap(),
            DateRange::new(Some(0), None).unwrap(),
            DateRange::new(None, None).unwrap(),
        ] {
            let text = range.to_text();
            assert_eq!(DateRange::from_text(&text), Some(range), "{text}");
        }
        let empty = DateRange::new(Some(5), Some(5)).unwrap();
        assert_eq!(empty.to_text(), "empty");
        assert!(DateRange::from_text("empty").is_some_and(|read| read.empty));
    }
}

/// A range of any subtype, with its bounds' **inclusivity as data**.
///
/// [`DateRange`] above it is the half-open date range an `EXCLUDE` key needs and nothing more;
/// this is what a `tsrange` column holds, and the difference is the whole reason both exist: a
/// `tsrange` round-trips the exact characters `[`, `]`, `(`, `)` it was written with, so a type
/// that assumed `[a,b)` would answer `["a","b")` for a value inserted as `["a","b"]`.
///
/// # What a reader would get wrong
///
/// * **A continuous subtype is not canonicalised.** `int4range '[1,10]'` comes back `[1,11)`
///   because an integer has a successor; `tsrange '[a,b]'` comes back `["a","b"]` because a
///   timestamp does not. An implementation that canonicalises everything or nothing is wrong
///   either way, and `pg_range.rngcanonical` is `-` for `tsrange` and `int4range_canonical` for
///   the other — measured.
/// * **`-infinity` is a *value*, not an unbounded bound.** `'[-infinity, infinity]'::tsrange` has
///   `lower_inf` and `upper_inf` both **false**, and prints with its brackets intact. Unbounded is
///   the *absent* bound — `'[a,]'` prints `["a",)`, closing with a parenthesis because there is
///   nothing there to include.
/// * **A zero-width range collapses to `empty`**: `[a,a)` is `empty` and `[a,a]` is not.
/// * **`empty` is a value and not NULL.** `isempty` is `t` for one and the whole value is NULL for
///   the other, and `range_test.rb` asserts both separately.
#[derive(Debug, Clone, PartialEq)]
pub struct Range {
    /// The empty range, which has no bounds at all and is not NULL.
    pub empty: bool,
    /// The lower bound, or `None` for unbounded below.
    pub lower: Option<Datum>,
    /// The upper bound, or `None` for unbounded above.
    pub upper: Option<Datum>,
    /// Whether the lower bound is included — the `[` or `(` as written, after canonicalisation.
    pub lower_inc: bool,
    /// Whether the upper bound is included.
    pub upper_inc: bool,
}

impl Range {
    /// The empty range.
    #[must_use]
    pub fn empty() -> Self {
        Range {
            empty: true,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
        }
    }

    /// The canonical text, which is what a client is sent and what the row holds.
    #[must_use]
    pub fn to_text(&self) -> String {
        if self.empty {
            return "empty".to_owned();
        }
        let bound = |value: Option<&Datum>| match value.and_then(PgDatum::to_text) {
            None => String::new(),
            Some(text) => quote_bound(&text),
        };
        format!(
            "{}{},{}{}",
            if self.lower_inc { '[' } else { '(' },
            bound(self.lower.as_ref()),
            bound(self.upper.as_ref()),
            if self.upper_inc { ']' } else { ')' },
        )
    }
}

/// Reads a range literal of `subtype`, or the error a real server gives for one it cannot.
///
/// **Three different SQLSTATEs, all measured**: a literal with no bracket is
/// `22P02 malformed range literal … DETAIL: Missing left parenthesis or bracket.`, one that stops
/// early is the same code with `DETAIL: Unexpected end of input.`, and bounds the wrong way round
/// are `22000 range lower bound must be less than or equal to range upper bound` — a *data*
/// exception rather than an input-syntax one, because the text parsed fine and the value is
/// impossible.
pub fn from_text(subtype: ColumnType, text: &str) -> Result<Range> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("empty") {
        return Ok(Range::empty());
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let lower_inc = match chars.first() {
        Some('[') => true,
        Some('(') => false,
        _ => return Err(malformed(text, "Missing left parenthesis or bracket.")),
    };
    let mut at = 1;
    let lower = bound(&chars, &mut at, text)?;
    if chars.get(at) != Some(&',') {
        return Err(malformed(text, "Unexpected end of input."));
    }
    at += 1;
    let upper = bound(&chars, &mut at, text)?;
    let upper_inc = match chars.get(at) {
        Some(']') => true,
        Some(')') => false,
        _ => return Err(malformed(text, "Unexpected end of input.")),
    };
    if at + 1 != chars.len() {
        return Err(malformed(text, "Junk after right parenthesis or bracket."));
    }
    let read = |value: Option<String>| match value {
        None => Ok(None),
        Some(text) => Datum::from_text(subtype, &text).map(Some),
    };
    let mut range = Range {
        empty: false,
        lower: read(lower)?,
        upper: read(upper)?,
        lower_inc,
        upper_inc,
    };
    canonicalise(subtype, &mut range)?;
    Ok(range)
}

/// The bounds a **discrete** subtype normalises to, and the emptiness every subtype collapses to.
///
/// **Every way of making a range goes through here**, which is the point of it being public: the
/// literal `'[3,1)'::tsrange` and the constructor `tsrange(hi, lo)` are two spellings of one value
/// and must answer alike. They did not — the literal was `22000` and the constructor built an
/// impossible range object, or, for `daterange`, quietly answered `empty`.
///
/// `int4range '[1,10]'` is `[1,11)`: an integer has a successor, so every range of them has one
/// spelling. A timestamp has none, so a `tsrange` keeps the brackets it was given — which is why
/// this takes the subtype rather than normalising unconditionally.
pub fn canonicalise(subtype: ColumnType, range: &mut Range) -> Result<()> {
    // **An absent bound is never inclusive**, whatever bracket was written beside it: `'[a,]'`
    // prints `["a",)`, because there is nothing there to include. Measured, and it is why the
    // bracket a client sees is not always the bracket it sent.
    range.lower_inc &= range.lower.is_some();
    range.upper_inc &= range.upper.is_some();
    if matches!(subtype, ColumnType::Int4 | ColumnType::Int8) {
        if let (Some(Datum::Int8(lower)), true) = (range.lower.clone(), !range.lower_inc) {
            range.lower = Some(Datum::Int8(lower.saturating_add(1)));
            range.lower_inc = true;
        }
        if let (Some(Datum::Int8(upper)), true) = (range.upper.clone(), range.upper_inc) {
            range.upper = Some(Datum::Int8(upper.saturating_add(1)));
            range.upper_inc = false;
        }
    }
    if let (Some(lower), Some(upper)) = (&range.lower, &range.upper) {
        match lower.pg_cmp(upper) {
            std::cmp::Ordering::Greater => {
                return Err(SqlError::RangeBoundsOutOfOrder);
            }
            // **A zero-width range collapses to `empty`** unless both ends are included: `[a,a)`
            // is empty and `[a,a]` is the single point.
            std::cmp::Ordering::Equal if !(range.lower_inc && range.upper_inc) => {
                *range = Range::empty();
            }
            _ => {}
        }
    }
    Ok(())
}

/// One bound's text, or `None` for an absent one — which is *unbounded*, and not the same thing as
/// an `-infinity` that happens to be written there.
fn bound(chars: &[char], at: &mut usize, whole: &str) -> Result<Option<String>> {
    if chars.get(*at) == Some(&'"') {
        *at += 1;
        let mut out = String::new();
        while let Some(ch) = chars.get(*at) {
            match ch {
                '\\' if *at + 1 < chars.len() => {
                    out.push(chars[*at + 1]);
                    *at += 2;
                }
                '"' => {
                    *at += 1;
                    return Ok(Some(out));
                }
                other => {
                    out.push(*other);
                    *at += 1;
                }
            }
        }
        return Err(malformed(whole, "Unexpected end of input."));
    }
    let start = *at;
    while let Some(ch) = chars.get(*at) {
        if matches!(ch, ',' | ']' | ')') {
            break;
        }
        *at += 1;
    }
    let text: String = chars[start..*at].iter().collect();
    let text = text.trim().to_owned();
    Ok((!text.is_empty()).then_some(text))
}

fn malformed(text: &str, detail: &'static str) -> SqlError {
    SqlError::MalformedRangeLiteral {
        value: text.to_owned(),
        detail,
    }
}

/// One bound, quoted only when it needs to be.
///
/// PostgreSQL quotes a bound that contains a character the literal grammar uses, and leaves the
/// rest bare — which is why a timestamp comes back `"2010-01-01 14:30:00"` and `-infinity` does
/// not. Quoting everything would round-trip and still print a value no real server prints.
fn quote_bound(text: &str) -> String {
    let needs = text.is_empty()
        || text
            .chars()
            .any(|ch| matches!(ch, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || ch.is_whitespace());
    if !needs {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod range_tests {
    //! Every case is a row of `tests/corpus/pg19_tsrange.txt`, kept beside the code because the
    //! corpus cannot run until a column can hold one and these can run now.

    use super::{Range, from_text};
    use crate::value::ColumnType;

    fn round(subtype: ColumnType, text: &str) -> String {
        from_text(subtype, text).unwrap().to_text()
    }

    /// **A continuous subtype keeps the brackets it was given**; a discrete one is normalised.
    /// One rule would be wrong for one of these two lines.
    #[test]
    fn a_timestamp_range_is_not_canonicalised_and_an_integer_range_is() {
        assert_eq!(
            round(
                ColumnType::Timestamp,
                "[2010-01-01 14:30, 2011-01-01 14:30]"
            ),
            r#"["2010-01-01 14:30:00","2011-01-01 14:30:00"]"#
        );
        assert_eq!(round(ColumnType::Int8, "[1, 10]"), "[1,11)");
        assert_eq!(round(ColumnType::Int8, "[1, 10)"), "[1,10)");
    }

    /// An **absent** bound is unbounded and closes with a parenthesis; `-infinity` is a *value*
    /// and keeps its bracket.
    #[test]
    fn unbounded_is_absent_and_infinity_is_a_value() {
        assert_eq!(
            round(ColumnType::Timestamp, "[2010-01-01 14:30,]"),
            r#"["2010-01-01 14:30:00",)"#
        );
        assert_eq!(round(ColumnType::Int8, "[1,]"), "[1,)");
        assert_eq!(round(ColumnType::Int8, "[,]"), "(,)");
        assert_eq!(
            round(ColumnType::Timestamp, "[-infinity, infinity]"),
            "[-infinity,infinity]"
        );
    }

    /// `empty` is a value, and a zero-width range becomes one unless both ends are included.
    #[test]
    fn a_zero_width_range_collapses_to_empty() {
        assert_eq!(round(ColumnType::Timestamp, "empty"), "empty");
        assert_eq!(
            round(
                ColumnType::Timestamp,
                "[2010-01-01 14:30, 2010-01-01 14:30)"
            ),
            "empty"
        );
        assert_eq!(
            round(
                ColumnType::Timestamp,
                "[2010-01-01 14:30, 2010-01-01 14:30]"
            ),
            r#"["2010-01-01 14:30:00","2010-01-01 14:30:00"]"#
        );
        assert_eq!(Range::empty().to_text(), "empty");
    }

    /// A BC year survives, which is the whole of `test_escaped_tsrange`.
    #[test]
    fn a_bc_year_round_trips() {
        assert_eq!(
            round(
                ColumnType::Timestamp,
                "[1000-01-01 14:30:00 BC, 2020-02-02 14:30]"
            ),
            r#"["1000-01-01 14:30:00 BC","2020-02-02 14:30:00"]"#
        );
    }

    /// **Three different SQLSTATEs**, and the reversed pair is the one that is not `22P02`.
    #[test]
    fn the_three_bad_literals_have_three_answers() {
        let reversed = from_text(
            ColumnType::Timestamp,
            "[2011-01-01 14:30, 2010-01-01 14:30]",
        )
        .unwrap_err();
        assert_eq!(reversed.sqlstate(), "22000");
        assert_eq!(
            reversed.to_string(),
            "range lower bound must be less than or equal to range upper bound"
        );

        let truncated =
            from_text(ColumnType::Timestamp, "[2010-01-01 14:30, 2011-01-01 14:30").unwrap_err();
        assert_eq!(truncated.sqlstate(), "22P02");
        assert_eq!(
            truncated.detail().as_deref(),
            Some("Unexpected end of input.")
        );

        let nonsense = from_text(ColumnType::Timestamp, "nonsense").unwrap_err();
        assert_eq!(nonsense.sqlstate(), "22P02");
        assert_eq!(
            nonsense.to_string(),
            "malformed range literal: \"nonsense\""
        );
        assert_eq!(
            nonsense.detail().as_deref(),
            Some("Missing left parenthesis or bracket.")
        );
    }
}
