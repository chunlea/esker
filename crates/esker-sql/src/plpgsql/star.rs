//! `NEW.*` — a record's fields written out, for the SQL a trigger body holds.
//!
//! `INSERT INTO postgresql_partitioned_table VALUES (NEW.*)` is the whole of statement 762's
//! trigger, and a `VALUES` list has no row expansion of its own here. So before the statement is
//! parsed, `NEW.*` becomes `NEW.id, NEW.number` — one reference per field, in the record's order,
//! which is PostgreSQL's reading of a whole-row reference in a list.

use super::lex::{self, Kind};

/// `text` with every `<record>.*` written out as the record's fields, for the records in `records`.
///
/// Only an unquoted record name followed by `.` and `*` is expanded; a `*` anywhere else is the
/// SQL's own. Text that does not tokenize comes back as it is, for the SQL layer to refuse in its
/// own words.
#[must_use]
pub fn expand_record_stars(text: &str, records: &[(&str, &[String])]) -> String {
    let Ok(tokens) = lex::tokenize(text) else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(&token) = tokens.get(at) {
        if token.kind == Kind::Word
            && let (Some(dot), Some(star)) = (tokens.get(at + 1), tokens.get(at + 2))
            && dot.is_punct(text, '.')
            && star.is_operator(text, "*")
            && let Some((_, fields)) = records
                .iter()
                .find(|(name, _)| token.text(text).eq_ignore_ascii_case(name))
        {
            out.push_str(text.get(copied..token.start).unwrap_or_default());
            let written: Vec<String> = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}.{}",
                        token.text(text),
                        crate::catalog::quote_identifier(field)
                    )
                })
                .collect();
            out.push_str(&written.join(", "));
            copied = star.end;
            at += 3;
            continue;
        }
        at += 1;
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    out
}

#[cfg(test)]
mod tests {
    use super::expand_record_stars;

    #[test]
    fn a_whole_row_reference_is_written_out_as_its_fields() {
        let fields = ["id".to_owned(), "Number".to_owned()];
        assert_eq!(
            expand_record_stars(
                "INSERT INTO postgresql_partitioned_table VALUES (NEW.*)",
                &[("new", &fields)]
            ),
            "INSERT INTO postgresql_partitioned_table VALUES (NEW.id, NEW.\"Number\")"
        );
    }

    /// A `*` that is not a record's is the SQL's, and so is a record the caller did not name.
    #[test]
    fn every_other_star_is_left_alone() {
        let fields = ["id".to_owned()];
        for text in [
            "SELECT * FROM t",
            "SELECT t.* FROM t",
            "SELECT 2 * 3",
            "SELECT 'NEW.*'",
        ] {
            assert_eq!(expand_record_stars(text, &[("new", &fields)]), text);
        }
        assert_eq!(
            expand_record_stars("SELECT OLD.*", &[("new", &fields)]),
            "SELECT OLD.*"
        );
    }
}
