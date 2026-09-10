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
            // **`{A,B,C}` is one of four input forms and the only output one.** The other three
            // are two *points* — `(x1,y1),(x2,y2)`, `[(x1,y1),(x2,y2)]` and the bare
            // `x1,y1,x2,y2` — which a real server converts to coefficients on the way in, so
            // `'(2,3),(4,6)'::line` prints `{1.5,-1,0}`. Measured, and the whole reason this arm
            // is not `Lseg`'s: a `line` is the only shape whose *output* spelling is a different
            // shape from its input.
            if let Some(inner) = trim_pair(body, '{', '}') {
                let parts = numbers(inner).ok_or_else(invalid)?;
                let [a, b, c] = parts[..] else {
                    return Err(invalid());
                };
                // **`A` and `B` cannot both be zero**, which is its own sentence and its own
                // check: `Ax + By + C = 0` names no line when both are.
                if a == 0.0 && b == 0.0 {
                    return Err(SqlError::InvalidLineSpecification);
                }
                return Ok(format!("{{{},{},{}}}", num(a), num(b), num(c)));
            }
            let parts = numbers(body).ok_or_else(invalid)?;
            let [x1, y1, x2, y2] = parts[..] else {
                return Err(invalid());
            };
            // One point named twice is no line, and it has a sentence of its own — a *different*
            // one from the flat `{0,0,0}` above, measured beside it.
            //
            // `clippy::float_cmp` wants a tolerance and there is none to have: PostgreSQL's own
            // check is `==`, so `(0,0),(1e-300,0)` is a line there and would not be one here if
            // this compared within a margin. Exactness is the behaviour, not an oversight.
            #[expect(
                clippy::float_cmp,
                reason = "PostgreSQL's own check is exact; see above"
            )]
            if x1 == x2 && y1 == y2 {
                return Err(SqlError::LineNeedsTwoPoints);
            }
            // **A vertical line is `{-1,0,x}`** and every other is `{m,-1,y1 - m·x1}`, where the
            // `C` keeps the sign of its zero: `'(0,-0),(1,-0)'::line` is `{0,-1,-0}` on a real
            // server, not `{0,-1,0}`.
            #[expect(clippy::float_cmp, reason = "a vertical line is x1 == x2 exactly")]
            let (a, b, c) = if x1 == x2 {
                (-1.0, 0.0, x1)
            } else {
                let slope = (y2 - y1) / (x2 - x1);
                (slope, -1.0, y1 - slope * x1)
            };
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
/// **How close two coordinates have to be to count as one.**
///
/// PostgreSQL's geometric operators are fuzzy and this is the constant: a point `1e-6` outside an
/// edge is contained and one `1e-5` outside is not, and `point_ne` draws the same line
/// (`tests/captures/pg19_point_ne.txt`, `pg19_polygon_contains.txt`). A predicate written against
/// the reals is wrong on every pair inside the band, which is not a rounding detail — it is what
/// the operator means.
pub(crate) const EPSILON: f64 = 1.0e-6;

/// A polygon's vertices, in order.
fn ring(text: &str) -> Option<Vec<(f64, f64)>> {
    let flat = numbers(text)?;
    (flat.len() >= 2 && flat.len() % 2 == 0).then(|| {
        flat.chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect()
    })
}

/// How far `point` is from the segment `a`–`b`.
fn distance_to_segment(point: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx.mul_add(dx, dy * dy);
    // A degenerate segment is a point, and the distance to it is the distance to that point.
    let t = if length <= 0.0 {
        0.0
    } else {
        (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    };
    let (nx, ny) = (dx.mul_add(t, a.0), dy.mul_add(t, a.1));
    (point.0 - nx).hypot(point.1 - ny)
}

/// Whether `point` is inside `ring`, **with the boundary counted as inside** and fuzzily so.
fn point_in_ring(ring: &[(f64, f64)], point: (f64, f64)) -> bool {
    // The boundary first, because the crossing count below is exactly what is undefined on it.
    for pair in 0..ring.len() {
        let (a, b) = (ring[pair], ring[(pair + 1) % ring.len()]);
        if distance_to_segment(point, a, b) <= EPSILON {
            return true;
        }
    }
    // Ray casting: an odd number of crossings to the right means inside.
    let mut inside = false;
    for pair in 0..ring.len() {
        let (a, b) = (ring[pair], ring[(pair + 1) % ring.len()]);
        if (a.1 > point.1) != (b.1 > point.1) {
            let at = (b.0 - a.0) * (point.1 - a.1) / (b.1 - a.1) + a.0;
            if point.0 < at {
                inside = !inside;
            }
        }
    }
    inside
}

/// Which side of the line `a`–`b` the point `c` is on, as a sign.
fn side(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Whether the segments `a`–`b` and `c`–`d` cross **properly** — sharing an endpoint or touching
/// along a boundary is not a crossing, which is what lets an inner polygon share an edge with its
/// container.
fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    // **Touching within the epsilon is not crossing**, and it has to be measured as a distance
    // rather than read off `side`, which is twice a triangle's area and so grows with the
    // segments. Without this, a polygon lying `1e-7` outside an edge has a side that strictly
    // crosses it and the containment came back `f` where PostgreSQL says `t` — one cell of
    // `pg19_polygon_contains.txt`, and the reason that cell is in it.
    if distance_to_segment(c, a, b) <= EPSILON
        || distance_to_segment(d, a, b) <= EPSILON
        || distance_to_segment(a, c, d) <= EPSILON
        || distance_to_segment(b, c, d) <= EPSILON
    {
        return false;
    }
    let (s1, s2) = (side(a, b, c), side(a, b, d));
    let (s3, s4) = (side(c, d, a), side(c, d, b));
    (s1 * s2 < 0.0) && (s3 * s4 < 0.0)
}

/// Whether two polygons are the same ring, as `~=` means.
///
/// **A polygon is the same as its own rotation and its own reversal.** The vertex list may start
/// anywhere and run either way; an arbitrary shuffle of the same points is a different polygon.
/// PostgreSQL does *not* normalise the text — a rotation prints differently and compares same — so
/// comparing canonical forms, which is the first thing a node that stores canonical text reaches
/// for, answers `f` on every rotation. Measured in `tests/captures/pg19_same_as.txt`.
pub(crate) fn polygons_same(left: &str, right: &str) -> Option<bool> {
    let (left, right) = (ring(left)?, ring(right)?);
    if left.len() != right.len() {
        return Some(false);
    }
    if left.is_empty() {
        return Some(true);
    }
    let matches_from = |reversed: bool, start: usize| {
        left.iter().enumerate().all(|(at, point)| {
            let index = if reversed {
                (start + right.len() - at) % right.len()
            } else {
                (start + at) % right.len()
            };
            same_point(*point, right[index])
        })
    };
    Some((0..right.len()).any(|start| matches_from(false, start) || matches_from(true, start)))
}

/// Whether two points are the same one, fuzzily — the shared epsilon, per coordinate.
///
/// **`NaN` is the same as `NaN`**, because every comparison against one is false and this is the
/// negation of "differs by more than the epsilon". `point_ne` is the same rule read the other way.
pub(crate) fn same_point(left: (f64, f64), right: (f64, f64)) -> bool {
    !((left.0 - right.0).abs() > EPSILON || (left.1 - right.1).abs() > EPSILON)
}

/// Whether two geometric values of the same kind are the same, as `~=` means.
pub(crate) fn same_as(kind: super::ColumnType, left: &str, right: &str) -> Option<bool> {
    if kind == super::ColumnType::Polygon {
        return polygons_same(left, right);
    }
    // Every other shape this node has is its vertices in order, and a point is one of them.
    let (left, right) = (ring(left)?, ring(right)?);
    (left.len() == right.len()).then(|| {
        left.iter()
            .zip(right.iter())
            .all(|(a, b)| same_point(*a, *b))
    })
}

/// Whether the polygon `outer` contains the polygon `inner`, as `@>` means.
///
/// **Two questions, and the second is the one a plausible implementation leaves out**: every vertex
/// of `inner` must be inside `outer`, *and* no edge of `inner` may cross an edge of `outer`. In a
/// concave `outer` an edge can leave and re-enter between two contained vertices, and PostgreSQL
/// answers `f` for exactly that — measured before this was written, which is why it is here.
pub(crate) fn polygon_contains(outer: &str, inner: &str) -> Option<bool> {
    let (outer, inner) = (ring(outer)?, ring(inner)?);
    if !inner.iter().all(|point| point_in_ring(&outer, *point)) {
        return Some(false);
    }
    for i in 0..inner.len() {
        let (a, b) = (inner[i], inner[(i + 1) % inner.len()]);
        for j in 0..outer.len() {
            let (c, d) = (outer[j], outer[(j + 1) % outer.len()]);
            if segments_cross(a, b, c, d) {
                return Some(false);
            }
        }
    }
    Some(true)
}

/// Whether two polygons overlap, as `&&` means — **touching counts**, measured: two squares
/// sharing only an edge overlap, and so do two sharing only a vertex.
pub(crate) fn polygons_overlap(left: &str, right: &str) -> Option<bool> {
    let (left, right) = (ring(left)?, ring(right)?);
    if left.iter().any(|point| point_in_ring(&right, *point))
        || right.iter().any(|point| point_in_ring(&left, *point))
    {
        return Some(true);
    }
    for i in 0..left.len() {
        let (a, b) = (left[i], left[(i + 1) % left.len()]);
        for j in 0..right.len() {
            let (c, d) = (right[j], right[(j + 1) % right.len()]);
            if segments_cross(a, b, c, d) {
                return Some(true);
            }
        }
    }
    Some(false)
}

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
        assert!(from_text(Kind::Line, "{1,2,3,4}").is_err());
        // **The three two-point spellings, all converted to coefficients** — measured on
        // 19beta1, one row at a time, because nothing about the output form suggests them.
        for (written, want) in [
            ("(2,3),(4,6)", "{1.5,-1,0}"),
            ("[(2,3),(4,6)]", "{1.5,-1,0}"),
            ("((2,3),(4,6))", "{1.5,-1,0}"),
            ("2,3,4,6", "{1.5,-1,0}"),
            (" (2,3) , (4,6) ", "{1.5,-1,0}"),
            // Horizontal, vertical, and a vertical at a negative x — the vertical form is
            // `{-1,0,x}` and its `C` is the *coordinate*, not its negation.
            ("(0,0),(1,0)", "{0,-1,0}"),
            ("(0,5),(3,5)", "{0,-1,5}"),
            ("(0,0),(0,1)", "{-1,0,0}"),
            ("(5,0),(5,3)", "{-1,0,5}"),
            ("(-5,0),(-5,3)", "{-1,0,-5}"),
            ("(-1,-2),(3,4)", "{1.5,-1,-0.5}"),
            ("(0.5,0.25),(1.5,2.75)", "{2.5,-1,-1}"),
            ("(0,1),(1,0)", "{-1,-1,1}"),
            ("(1,-1),(2,-2)", "{-1,-1,0}"),
            ("(3,4),(1,2)", "{1,-1,1}"),
            // **The order of the two points does not matter**, which the slope makes true and
            // the capture confirms.
            ("(0,0),(1,3)", "{3,-1,0}"),
            ("(1,3),(0,0)", "{3,-1,0}"),
            // **`C` keeps the sign of its zero**: a real server prints `-0` here.
            ("(0,-0),(1,-0)", "{0,-1,-0}"),
        ] {
            assert_eq!(from_text(Kind::Line, written).unwrap(), want, "{written}");
        }
        // One point named twice is its own sentence, and not the `{0,0,0}` one below it.
        let same = from_text(Kind::Line, "(2,3),(2,3)").unwrap_err();
        assert_eq!(same.sqlstate(), "22P02");
        assert_eq!(
            same.to_string(),
            "invalid line specification: must be two distinct points"
        );
        for wrong in ["(2,3)", "[(2,3)]", "nonsense", ""] {
            let refused = from_text(Kind::Line, wrong).unwrap_err();
            assert_eq!(refused.sqlstate(), "22P02", "{wrong}");
            assert_eq!(
                refused.to_string(),
                format!("invalid input syntax for type line: \"{wrong}\""),
                "{wrong}"
            );
        }
        let flat = from_text(Kind::Line, "{0,0,0}").unwrap_err();
        assert_eq!(flat.sqlstate(), "22P02");
        assert_eq!(
            flat.to_string(),
            "invalid line specification: A and B cannot both be zero"
        );
    }
}

#[cfg(test)]
mod contains_tests {
    /// **Every `~=` cell**, likewise read out of its capture.
    #[test]
    fn every_measured_same_as_cell_agrees() {
        let capture = include_str!("../../tests/captures/pg19_same_as.txt");
        let mut checked = 0;
        for line in capture.lines().filter(|line| !line.starts_with('#')) {
            let mut fields = line.split('\t');
            let (Some(statement), Some(_), Some(expected)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(inner) = statement
                .strip_prefix("SELECT ('")
                .and_then(|rest| rest.strip_suffix(") AS v"))
            else {
                continue;
            };
            let Some((left, rest)) = inner.split_once("'::polygon ~= '") else {
                continue;
            };
            let Some(right) = rest.strip_suffix("'::polygon") else {
                continue;
            };
            assert_eq!(
                super::polygons_same(left, right),
                Some(expected == "t"),
                "{left} ~= {right}"
            );
            checked += 1;
        }
        assert!(checked >= 8, "only {checked} cells read");
    }

    /// **Every measured cell of the capture**, read rather than restated.
    ///
    /// The capture is the specification — `value::geometric` had no predicates at all before this,
    /// so there was nothing to derive them from but PostgreSQL's answers.
    #[test]
    fn every_measured_polygon_cell_agrees() {
        let capture = include_str!("../../tests/captures/pg19_polygon_contains.txt");
        let mut checked = 0;
        for line in capture.lines().filter(|line| !line.starts_with('#')) {
            let mut fields = line.split('\t');
            let (Some(statement), Some(_), Some(expected)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(inner) = statement
                .strip_prefix("SELECT ('")
                .and_then(|rest| rest.strip_suffix(") AS v"))
            else {
                continue;
            };
            let Some((left, rest)) = inner.split_once("'::polygon ") else {
                continue;
            };
            let Some((op, right)) = rest.split_once(" '") else {
                continue;
            };
            let ours = if let Some(right) = right.strip_suffix("'::polygon") {
                match op {
                    "@>" => super::polygon_contains(left, right),
                    // `<@` is `@>` with the operands the other way round, measured.
                    "<@" => super::polygon_contains(right, left),
                    "&&" => super::polygons_overlap(left, right),
                    _ => continue,
                }
            } else if let Some(right) = right.strip_suffix("'::point") {
                // A point is a one-vertex ring, so the same predicate answers for it.
                match op {
                    "@>" => super::polygon_contains(left, right),
                    _ => continue,
                }
            } else {
                continue;
            };
            assert_eq!(
                ours,
                Some(expected == "t"),
                "{left} {op} {right} -- PostgreSQL says {expected}"
            );
            checked += 1;
        }
        assert!(
            checked > 35,
            "only {checked} cells read; the capture did not load"
        );
    }
}
