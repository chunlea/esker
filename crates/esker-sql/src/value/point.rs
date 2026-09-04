//! `point` — two `float8` coordinates, read and written as `(x,y)`.
//!
//! **The input is looser than the output.** A real server accepts `(1,2)`, `( 1 , 2 )` and a bare
//! `1,2`, and prints all three as `(1,2)`: the parentheses belong to the output function, not to
//! the parser. Measured, all three.
//!
//! The coordinates print exactly as a `float8` does, which is why this module formats through
//! the crate's own float formatter rather than through `{}`: `1e10` is `10000000000` and `2e-10`
//! stays `2e-10`, and a formatter of its own would get one of those two wrong. (Written plainly
//! rather than linked: `value::float` is `pub(crate)`, and a public doc may not link to one.)

use crate::error::{Result, SqlError};

/// `(x,y)`, as a real server's output function writes it.
#[must_use]
pub fn to_text(x: f64, y: f64) -> String {
    format!(
        "({},{})",
        super::float::to_text(x),
        super::float::to_text(y)
    )
}

/// The two coordinates a literal holds.
///
/// **`22P02` quotes the whole literal**, not the part that failed — `'(1,2,3)'::point` is
/// `invalid input syntax for type point: "(1,2,3)"`, measured, and so is `'(1)'` and `'x'`. A
/// message naming the offending coordinate would be a better error and a wrong one.
pub fn from_text(text: &str) -> Result<(f64, f64)> {
    let bad = || SqlError::InvalidTextRepresentation {
        ty: "point",
        value: text.to_owned(),
    };
    let trimmed = text.trim();
    // The parentheses are optional on input and are all-or-nothing: `(1,2` is not a point.
    let inner = match (trimmed.strip_prefix('('), trimmed.strip_suffix(')')) {
        (Some(_), Some(_)) => trimmed[1..trimmed.len() - 1].trim(),
        (None, None) => trimmed,
        _ => return Err(bad()),
    };
    let (left, right) = inner.split_once(',').ok_or_else(bad)?;
    // Exactly two: `(1,2,3)` is `22P02` rather than a point that ignores the third.
    if right.contains(',') {
        return Err(bad());
    }
    let coordinate = |part: &str| super::float::from_text(part.trim()).map_err(|_| bad());
    Ok((coordinate(left)?, coordinate(right)?))
}

#[cfg(test)]
mod tests {
    /// The three spellings a real server accepts, and the one form it prints.
    #[test]
    fn the_parentheses_are_optional_on_input_and_written_on_output() {
        for text in ["(1,2)", "( 1 , 2 )", "1,2", "  (1,2)  "] {
            let (x, y) = super::from_text(text).unwrap();
            assert_eq!(super::to_text(x, y), "(1,2)", "{text}");
        }
    }

    /// And the coordinates print as `float8` does, which is the reason this is not `{}`.
    #[test]
    fn a_coordinate_prints_as_a_double_precision() {
        let (x, y) = super::from_text("(1e10,2e-10)").unwrap();
        assert_eq!(super::to_text(x, y), "(10000000000,2e-10)");
    }

    /// Every shape a real server answers `22P02` for, quoting the whole literal.
    #[test]
    fn a_literal_that_is_not_two_numbers_is_refused() {
        for text in ["x", "(1)", "(1,2,3)", "(1,2", "1,2)", "", "(,)"] {
            assert!(super::from_text(text).is_err(), "{text} was accepted");
        }
    }
}
