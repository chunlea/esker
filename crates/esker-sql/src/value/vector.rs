//! An array read from its **own text form**, for the catalog columns that hold one.
//!
//! # Why an array is not a [`Datum`](crate::value::Datum)
//!
//! Every array this node answers with is a *catalog* answer — `pg_index.indkey`,
//! `pg_constraint.conkey`, `current_schemas(false)` — and none of them can be stored: a column of
//! an array type is still `0A000`. Three things follow, and together they decide the
//! representation:
//!
//! * **`Datum` is the row codec's type**, in `esker-keys`. A variant there would need an arm in
//!   `esker_keys::row` and in the columnar decoder for a value neither can ever see.
//! * **A `ColumnType` would advertise a column type this node refuses.** `pg_type`'s rows are
//!   derived from `ColumnType::ALL`, so an array type there tells a client `CREATE TABLE t (a
//!   int[])` will work, and it does not (`catalog::pg_catalog`'s module doc, on why `pg_range` is
//!   empty).
//! * **The text form *is* the value.** `int2vectorout` writes `2 3` and nothing else — no quoting,
//!   no NULLs, no nesting — and `ActiveRecord` reads the column with `String#split(" ")`. The
//!   catalog already holds exactly those bytes and `indkey::text` is already byte-identical to a
//!   real server's.
//!
//! So an array here is a `text` value that the array **operators** read, and the declared type is
//! the standing `pg_catalog` divergence every other column of these views already carries.
//!
//! # The two lower bounds, which is the whole of this module
//!
//! `int2vector` is **0-based** and an ordinary array is **1-based**. Measured:
//! `array_position(indkey, <first column>)` is `0` and `array_position('{a,b,c}'::text[], 'a')` is
//! `1`. An implementation that assumed one bound sorts identically under `ORDER BY` — which is all
//! boot statement 17 does with it — and is wrong for every caller that reads the number.

/// One array, as its text form gives it up.
#[derive(Debug, PartialEq, Eq)]
pub struct Array {
    /// The elements in order, `None` for a SQL NULL element.
    pub elements: Vec<Option<String>>,
    /// The subscript of the first element: `1` for an ordinary array, `0` for an `int2vector`.
    pub lower: i32,
}

impl Array {
    /// Reads an array out of the text a catalog column holds.
    ///
    /// The brace form is an ordinary array and the bare form is an `int2vector`, which is the only
    /// distinction that exists at this level and the only one that matters: they differ in their
    /// lower bound, and everything else about them is the same list.
    ///
    /// Deliberately narrow. `array_in`'s full grammar — nesting, explicit bounds, backslash
    /// escapes, `Incorrectly quoted array element.` — belongs with the array *type*, which this
    /// node does not have; every value that reaches here was written by this crate's own catalog.
    /// A shape it cannot read is `None` rather than a guess, and the caller answers `42883` the
    /// way a real server answers a function it has no overload for.
    #[must_use]
    pub fn read(text: &str) -> Option<Array> {
        let trimmed = text.trim();
        let Some(inner) = trimmed.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else {
            // `int2vectorout`: space-separated, 0-based, and empty for an index with no columns.
            return Some(Array {
                elements: trimmed
                    .split_whitespace()
                    .map(|part| Some(part.to_owned()))
                    .collect(),
                lower: 0,
            });
        };
        if inner.trim().is_empty() {
            return Some(Array {
                elements: Vec::new(),
                lower: 1,
            });
        }
        let mut elements = Vec::new();
        for (part, quoted) in split_top_level(inner)? {
            // **An unquoted `NULL` is a SQL NULL and a quoted `"NULL"` is the four characters**,
            // so whether the element was quoted has to survive the split — it cannot be recovered
            // from the text afterwards, because the quotes are gone by then.
            elements.push(if !quoted && part.trim().eq_ignore_ascii_case("null") {
                None
            } else if quoted {
                Some(part)
            } else {
                // Unquoted whitespace around an element is trimmed; quoted whitespace is kept.
                Some(part.trim().to_owned())
            });
        }
        Some(Array { elements, lower: 1 })
    }

    /// The `{a,b}` text a real server prints, quoting **only what needs it**.
    ///
    /// `array_out`'s rule, measured: `{a,b}` but `{"a b","c,d"}`, `{NULL,"NULL"}` and `{""}`. An
    /// element is quoted when leaving it bare would read back as something else — when it is
    /// empty, when it holds a delimiter, a brace, a quote, a backslash or whitespace, or when it
    /// spells `NULL`, which unquoted is the SQL NULL.
    #[must_use]
    pub fn write(elements: &[Option<String>]) -> String {
        let mut out = String::from("{");
        for (at, element) in elements.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            match element {
                None => out.push_str("NULL"),
                Some(text) => {
                    let bare = !text.is_empty()
                        && !text.eq_ignore_ascii_case("null")
                        && !text.chars().any(|c| {
                            c.is_whitespace() || matches!(c, ',' | '{' | '}' | '"' | '\\')
                        });
                    if bare {
                        out.push_str(text);
                    } else {
                        out.push('"');
                        for character in text.chars() {
                            if matches!(character, '"' | '\\') {
                                out.push('\\');
                            }
                            out.push(character);
                        }
                        out.push('"');
                    }
                }
            }
        }
        out.push('}');
        out
    }

    /// The subscript `value` sits at, or `None` when it is not there — `array_position`.
    #[must_use]
    pub fn position_of(&self, value: &str) -> Option<i32> {
        self.elements
            .iter()
            .position(|element| element.as_deref() == Some(value))
            .and_then(|at| i32::try_from(at).ok())
            .map(|at| self.lower + at)
    }

    /// `array_upper(a, 1)`: the last subscript, or `None` for an empty array — which has no
    /// dimensions at all, so its bounds and its length are NULL while its cardinality is 0.
    #[must_use]
    pub fn upper(&self) -> Option<i32> {
        i32::try_from(self.elements.len())
            .ok()
            .filter(|length| *length > 0)
            .map(|length| self.lower + length - 1)
    }
}

/// Splits on commas that are not inside double quotes, saying of each element **whether it was
/// quoted**.
///
/// The flag is not a convenience: the quotes are removed here, so by the time an element is a
/// `String` there is nothing left to tell `NULL` from `"NULL"`, and those are a SQL NULL and a
/// four-character string.
fn split_top_level(inner: &str) -> Option<Vec<(String, bool)>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut was_quoted = false;
    let mut quoted = false;
    let mut escaped = false;
    for character in inner.chars() {
        match character {
            _ if escaped => {
                current.push(character);
                escaped = false;
            }
            '\\' if quoted => escaped = true,
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            ',' if !quoted => {
                parts.push((std::mem::take(&mut current), was_quoted));
                was_quoted = false;
            }
            // Nesting is the one shape this reader will not guess at.
            '{' | '}' if !quoted => return None,
            _ => current.push(character),
        }
    }
    if quoted {
        return None;
    }
    parts.push((current, was_quoted));
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::Array;

    /// The two lower bounds, which is what this module exists for.
    #[test]
    fn an_int2vector_is_zero_based_and_an_array_is_one_based() {
        let vector = Array::read("2 3").unwrap();
        assert_eq!(vector.lower, 0);
        assert_eq!(vector.position_of("2"), Some(0));
        assert_eq!(vector.position_of("3"), Some(1));
        assert_eq!(vector.position_of("9"), None);
        assert_eq!(vector.upper(), Some(1));

        let array = Array::read("{a,b,c}").unwrap();
        assert_eq!(array.lower, 1);
        assert_eq!(array.position_of("a"), Some(1));
        assert_eq!(array.upper(), Some(3));
    }

    /// An empty array of either spelling has no dimensions: no bounds, and no length.
    #[test]
    fn an_empty_array_has_no_upper_bound() {
        assert_eq!(Array::read("{}").unwrap().elements.len(), 0);
        assert_eq!(Array::read("{}").unwrap().upper(), None);
        assert_eq!(Array::read("").unwrap().elements.len(), 0);
        assert_eq!(Array::read("").unwrap().upper(), None);
    }

    /// An unquoted `NULL` is a SQL NULL and a quoted one is the four characters.
    #[test]
    fn a_quoted_null_is_the_word() {
        let array = Array::read("{NULL,\"NULL\"}").unwrap();
        assert_eq!(array.elements, [None, Some("NULL".to_owned())]);
    }

    /// What is written is what is read back, and only what needs quoting gets it.
    #[test]
    fn the_text_round_trips_and_quotes_only_what_needs_it() {
        for (elements, text) in [
            (vec![Some("a".to_owned()), Some("b".to_owned())], "{a,b}"),
            (
                vec![Some("a b".to_owned()), Some("c,d".to_owned())],
                "{\"a b\",\"c,d\"}",
            ),
            (vec![None, Some("NULL".to_owned())], "{NULL,\"NULL\"}"),
            (vec![Some(String::new())], "{\"\"}"),
            (Vec::new(), "{}"),
        ] {
            assert_eq!(Array::write(&elements), text, "writing {elements:?}");
            assert_eq!(
                Array::read(text).unwrap().elements,
                elements,
                "reading {text}"
            );
        }
    }

    /// A comma inside quotes is part of the element, and a nested array is refused rather than
    /// guessed at.
    #[test]
    fn quoting_holds_and_nesting_does_not() {
        let array = Array::read("{\"a b\",\"c,d\"}").unwrap();
        assert_eq!(
            array.elements,
            [Some("a b".to_owned()), Some("c,d".to_owned())]
        );
        assert_eq!(Array::read("{{1,2},{3,4}}"), None);
    }
}
