//! `lseg`, `box`, `path`, `polygon`, `circle` and `line`: six shapes, one canonical text each.
//!
//! # Every one of them reads more spellings than it writes
//!
//! Measured, one shape at a time — `geometric_test.rb` inserts each type twice, in two different
//! spellings, and asserts one answer:
//!
//! | type | read | written |
//! |---|---|---|
//! | `lseg` | `(2,3),(5.5,7)`, `[(2,3),(5.5,7)]`, `2,3,5.5,7` | `[(2,3),(5.5,7)]` |
//! | `box` | any two corners, bracketed or bare | `(5.5,7),(2,3)` |
//! | `path` | `[…]` **open**, `(…)` or bare **closed** | the bracket it is |
//! | `polygon` | `((…))` or a bare number list | `((2,3),(5.5,7),(8.5,11))` |
//! | `circle` | `<(x,y),r>`, `((x,y),r)`, `(x,y),r`, `x,y,r` | `<(5.3,10.4),2>` |
//! | `line` | `{A,B,C}` | `{2,3,5.5}` |
//!
//! **A `box` reorders its corners**: upper right first, then lower left, whatever order it was
//! given — `'2,3,5.5,7'::box` and `'(5.5,7),(2,3)'::box` both print `(5.5,7),(2,3)`. That is the
//! one rule a reader would not guess, and `geometric_test.rb` has a comment on it.
//!
//! **A `path`'s bracket is data**: `[…]` is open and `(…)` is closed, `isopen`/`isclosed` report
//! which, and the output keeps it. A bare list of points is *closed*.
//!
//! # None of the six is an index key
//!
//! `CREATE INDEX` on an `lseg` column is
//! `42704 data type lseg has no default operator class for access method "btree"` and
//! `count(DISTINCT a_line_segment)` is `42883 could not identify an equality operator for type
//! lseg` — even though the `=` **operator** exists and answers. Same shape as `point`
//! (ADR 0042), one type family along.

use crate::error::{Result, SqlError};

/// Which of the six a value is. The value carries it because a folded constant would otherwise
/// lose it — the lesson `Datum::Hstore` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `lseg`, written `[(x1,y1),(x2,y2)]`.
    Lseg,
    /// `box`, written `(upper right),(lower left)`.
    Box,
    /// `path`, written `[…]` when open and `(…)` when closed.
    Path,
    /// `polygon`, written `((…))`.
    Polygon,
    /// `circle`, written `<(x,y),r>`.
    Circle,
    /// `line`, written `{A,B,C}`.
    Line,
}

impl Kind {
    /// The name PostgreSQL's error message uses, which is the type's own.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Kind::Lseg => "lseg",
            Kind::Box => "box",
            Kind::Path => "path",
            Kind::Polygon => "polygon",
            Kind::Circle => "circle",
            Kind::Line => "line",
        }
    }
}

/// Reads one shape and answers its **canonical text**, which is what is stored.
///
/// The text is the value here, the road `hstore` and the ranges take: two shapes that print the
/// same are the same shape, so equality and grouping are the text's — and the canonicalisation
/// (the `box`'s reordering, the `path`'s bracket, a `float8`'s spelling) all happens once, on the
/// way in.
pub fn from_text(kind: Kind, text: &str) -> Result<String> {
    let invalid = || SqlError::InvalidTextRepresentation {
        ty: kind.name(),
        value: text.to_owned(),
    };
    let body = text.trim();
    match kind {
        Kind::Line => {
            let inner = body
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
                .ok_or_else(invalid)?;
            let parts = numbers(inner).ok_or_else(invalid)?;
            let [a, b, c] = parts[..] else {
                return Err(invalid());
            };
            // **`A` and `B` cannot both be zero**, which is its own sentence and its own check:
            // `Ax + By + C = 0` names no line when both are.
            if a == 0.0 && b == 0.0 {
                return Err(SqlError::InvalidLineSpecification);
            }
            Ok(format!("{{{},{},{}}}", num(a), num(b), num(c)))
        }
        Kind::Circle => {
            let inner = trim_pair(body, '<', '>')
                .or_else(|| trim_pair(body, '(', ')'))
                .unwrap_or(body);
            let parts = numbers(inner).ok_or_else(invalid)?;
            let [x, y, r] = parts[..] else {
                return Err(invalid());
            };
            Ok(format!("<({},{}),{}>", num(x), num(y), num(r)))
        }
        Kind::Lseg => {
            let inner = trim_pair(body, '[', ']').unwrap_or(body);
            let parts = numbers(inner).ok_or_else(invalid)?;
            let [x1, y1, x2, y2] = parts[..] else {
                return Err(invalid());
            };
            Ok(format!(
                "[({},{}),({},{})]",
                num(x1),
                num(y1),
                num(x2),
                num(y2)
            ))
        }
        Kind::Box => {
            let parts = numbers(body).ok_or_else(invalid)?;
            let [x1, y1, x2, y2] = parts[..] else {
                return Err(invalid());
            };
            // **The upper right corner first**, whatever order the two were written in.
            let (high_x, low_x) = if x1 >= x2 { (x1, x2) } else { (x2, x1) };
            let (high_y, low_y) = if y1 >= y2 { (y1, y2) } else { (y2, y1) };
            Ok(format!(
                "({},{}),({},{})",
                num(high_x),
                num(high_y),
                num(low_x),
                num(low_y)
            ))
        }
        Kind::Path | Kind::Polygon => {
            // **A `path`'s bracket says whether it is open**, and a bare list is closed. A
            // `polygon` is always written closed whatever it was given.
            let open = kind == Kind::Path && body.starts_with('[');
            let inner = trim_pair(body, '[', ']')
                .or_else(|| trim_pair(body, '(', ')'))
                .unwrap_or(body);
            let parts = numbers(inner).ok_or_else(invalid)?;
            if parts.is_empty() || parts.len() % 2 != 0 {
                return Err(invalid());
            }
            let points: Vec<String> = parts
                .chunks(2)
                .map(|pair| format!("({},{})", num(pair[0]), num(pair[1])))
                .collect();
            let joined = points.join(",");
            Ok(if open {
                format!("[{joined}]")
            } else {
                format!("({joined})")
            })
        }
    }
}

/// The numbers in a shape's text, in order, ignoring every bracket and comma between them.
///
/// One reader for all six because that is what PostgreSQL's own are: `'2,3,5.5,7'::box` and
/// `'(2,3),(5.5,7)'::box` are the same box, so the punctuation carries no information beyond the
/// outermost pair — which each arm above has already read.
fn numbers(text: &str) -> Option<Vec<f64>> {
    let mut out = Vec::new();
    for part in text.split([',', '(', ')', '[', ']', '<', '>']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(super::float::from_text(part).ok()?);
    }
    (!out.is_empty()).then_some(out)
}

/// The text between a matching pair of delimiters, or `None` if it is not wrapped in them.
fn trim_pair(text: &str, open: char, close: char) -> Option<&str> {
    text.strip_prefix(open)?.strip_suffix(close)
}

/// One coordinate, printed the way a `float8` is — `2.0` is `2` and `5.5` stays `5.5`.
fn num(value: f64) -> String {
    super::float::to_text(value)
}

#[cfg(test)]
mod tests {
    use super::{Kind, from_text};

    /// Every row of the module's own table, and both spellings `geometric_test.rb` inserts.
    #[test]
    fn each_shape_reads_more_spellings_than_it_writes() {
        for (kind, written, read_back) in [
            (Kind::Lseg, "(2.0, 3), (5.5, 7.0)", "[(2,3),(5.5,7)]"),
            (Kind::Lseg, "((2.0, 3), (5.5, 7.0))", "[(2,3),(5.5,7)]"),
            (Kind::Lseg, "2,3,5.5,7", "[(2,3),(5.5,7)]"),
            (Kind::Box, "2.0, 3, 5.5, 7.0", "(5.5,7),(2,3)"),
            (Kind::Box, "(2.0, 3), (5.5, 7.0)", "(5.5,7),(2,3)"),
            (Kind::Box, "(5.5,7),(2,3)", "(5.5,7),(2,3)"),
            (
                Kind::Path,
                "[(2.0, 3), (5.5, 7.0), (8.5, 11.0)]",
                "[(2,3),(5.5,7),(8.5,11)]",
            ),
            (
                Kind::Path,
                "((2.0, 3), (5.5, 7.0), (8.5, 11.0))",
                "((2,3),(5.5,7),(8.5,11))",
            ),
            (
                Kind::Polygon,
                "((2.0, 3), (5.5, 7.0), (8.5, 11.0))",
                "((2,3),(5.5,7),(8.5,11))",
            ),
            (
                Kind::Polygon,
                "2.0, 3, 5.5, 7.0, 8.5, 11.0",
                "((2,3),(5.5,7),(8.5,11))",
            ),
            (Kind::Circle, "<(5.3, 10.4), 2>", "<(5.3,10.4),2>"),
            (Kind::Circle, "((5.3, 10.4), 2)", "<(5.3,10.4),2>"),
            (Kind::Circle, "5.3,10.4,2", "<(5.3,10.4),2>"),
            (Kind::Line, "{2.0, 3, 5.5}", "{2,3,5.5}"),
        ] {
            assert_eq!(
                from_text(kind, written).unwrap(),
                read_back,
                "{kind:?} {written}"
            );
        }
    }

    /// The two refusals, and they are two different sentences.
    #[test]
    fn a_line_needs_a_and_b_and_nonsense_is_nonsense() {
        let nonsense = from_text(Kind::Lseg, "nonsense").unwrap_err();
        assert_eq!(nonsense.sqlstate(), "22P02");
        assert_eq!(
            nonsense.to_string(),
            "invalid input syntax for type lseg: \"nonsense\""
        );
        assert!(from_text(Kind::Line, "{1,2}").is_err());
        let flat = from_text(Kind::Line, "{0,0,0}").unwrap_err();
        assert_eq!(flat.sqlstate(), "22P02");
        assert_eq!(
            flat.to_string(),
            "invalid line specification: A and B cannot both be zero"
        );
    }
}
