//! `regnamespace`: an oid that prints as a **schema's** name — [ADR 0115], the second of ADR 0098's
//! kind, and what `check_all_foreign_keys_valid!` compares `pg_constraint.connamespace` through.
//!
//! OID 4089, `typlen` 4, `typcategory` `N`, `typinput` `regnamespacein`, `typarray` 4090.
//!
//! **The rules are `regnamespacein`'s and `regnamespaceout`'s, measured on 19beta1**
//! (`tests/corpus/pg19_regnamespace.txt`):
//!
//! * The string is trimmed and read as **exactly one identifier** — folded unless it is quoted, `""`
//!   inside the quotes being one quote. Anything else (`'a.b'`, `'S2 Mixed'`, `''`) is
//!   `42602 invalid name syntax`, and a well-formed name no schema has is
//!   `3F000 schema "nosuch" does not exist`. `to_regnamespace` answers `NULL` for both.
//! * **Digits are an oid**, whatever schema has it, and `-` is oid 0.
//! * A schema prints as `quote_ident` quotes its name, an oid no schema has prints its digits, and
//!   oid 0 prints `-`.
//!
//! **Nothing here reads the catalog.** Every function takes the tenant's schemas as the executor
//! read them (`catalog::View::schema_names`, the numbering `pg_namespace.oid` shows), so a name
//! resolved before the plan and one resolved per row are read by one reader and cannot disagree.
//!
//! [ADR 0115]: ../../../../docs/adr/0115-regnamespace-is-an-oid-that-prints-as-a-schema.md

use crate::error::{Result, SqlError};
use crate::value::Datum;

/// What a string turned out to be, for the two callers that answer a miss differently.
enum Read {
    /// A schema, or an oid spelled in digits.
    Found(Datum),
    /// A well-formed name no schema has, folded as it was looked up.
    Missing(String),
}

/// A `regnamespace` for an oid, printed as the schema that has it — or as its digits when none
/// does, and `-` for oid 0.
#[must_use]
pub fn of_oid(schemas: &[(String, u64)], oid: u32) -> Datum {
    match schemas.iter().find(|(_, id)| *id == u64::from(oid)) {
        Some((name, _)) if oid != 0 => Datum::RegNamespace {
            oid,
            name: crate::catalog::quote_identifier(name).into_boxed_str(),
        },
        _ => unnamed(oid),
    }
}

/// A `regnamespace` whose schema is not known here: its digits, or `-` for oid 0 — what
/// `regnamespaceout` prints for an oid no schema has.
#[must_use]
pub fn unnamed(oid: u32) -> Datum {
    Datum::RegNamespace {
        oid,
        name: if oid == 0 {
            "-".into()
        } else {
            oid.to_string().into_boxed_str()
        },
    }
}

/// `regnamespacein`: a string, as the schema it names, or PostgreSQL's `42602` or `3F000`.
///
/// # Errors
///
/// `42602` for a string that is not exactly one name, `3F000` for a name no schema has.
pub fn from_text(schemas: &[(String, u64)], text: &str) -> Result<Datum> {
    match read(schemas, text)? {
        Read::Found(datum) => Ok(datum),
        Read::Missing(name) => Err(SqlError::UndefinedSchema(name)),
    }
}

/// `to_regnamespace`: the same reading, and `None` wherever the cast raises — a malformed name as
/// much as a missing one, measured.
#[must_use]
pub fn try_from_text(schemas: &[(String, u64)], text: &str) -> Option<Datum> {
    match read(schemas, text) {
        Ok(Read::Found(datum)) => Some(datum),
        Ok(Read::Missing(_)) | Err(_) => None,
    }
}

/// The value layer's reading, with no schemas to ask: digits and `-` are an oid, and a name is
/// refused rather than guessed.
///
/// # Errors
///
/// `0A000` for anything that is not an oid, which only a caller holding the schemas can read.
pub fn without_a_catalog(text: &str) -> Result<Datum> {
    oid_spelled(text.trim()).map(unnamed).ok_or_else(|| {
        SqlError::unsupported("a schema name read as a regnamespace without a catalog")
    })
}

fn read(schemas: &[(String, u64)], text: &str) -> Result<Read> {
    let trimmed = text.trim();
    if let Some(oid) = oid_spelled(trimmed) {
        return Ok(Read::Found(of_oid(schemas, oid)));
    }
    let name = one_identifier(trimmed).ok_or(SqlError::InvalidNameSyntax)?;
    let Some((schema, id)) = schemas.iter().find(|(schema, _)| *schema == name) else {
        return Ok(Read::Missing(name));
    };
    let oid = u32::try_from(*id).map_err(|_| {
        SqlError::Internal(format!(
            "schema \"{schema}\" has id {id}, which no oid holds"
        ))
    })?;
    Ok(Read::Found(Datum::RegNamespace {
        oid,
        name: crate::catalog::quote_identifier(schema).into_boxed_str(),
    }))
}

/// An oid as `regnamespacein` spells one: digits, or `-` for 0.
fn oid_spelled(text: &str) -> Option<u32> {
    if text == "-" {
        return Some(0);
    }
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The one identifier `text` is, folded the way the parser folds one — or `None` when it is not
/// exactly one: empty, dotted, spaced, or a quote left open.
fn one_identifier(text: &str) -> Option<String> {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.chars();
        loop {
            match chars.next()? {
                '"' if chars.clone().next() == Some('"') => {
                    chars.next();
                    name.push('"');
                }
                '"' => break,
                other => name.push(other),
            }
        }
        return (chars.as_str().is_empty() && !name.is_empty()).then_some(name);
    }
    if text.is_empty() || text.contains(|c: char| c.is_whitespace() || c == '.' || c == '"') {
        return None;
    }
    Some(crate::catalog::fold_identifier(text, false).0)
}

#[cfg(test)]
mod tests {
    use super::{from_text, of_oid, try_from_text, without_a_catalog};
    use crate::value::Datum;

    fn schemas() -> Vec<(String, u64)> {
        vec![
            ("S2 Mixed".to_owned(), 40),
            ("pg_catalog".to_owned(), 12),
            ("public".to_owned(), 11),
            ("s2ns".to_owned(), 41),
        ]
    }

    fn printed(datum: &Datum) -> String {
        match datum {
            Datum::RegNamespace { name, .. } => name.to_string(),
            other => format!("{other:?}"),
        }
    }

    /// **One identifier, folded unless quoted**: `'PUBLIC'` is `public`, a leading space is
    /// trimmed, and a quoted mixed-case name prints back quoted — measured, all three.
    #[test]
    fn a_name_reads_as_one_identifier() {
        let schemas = schemas();
        for (written, expected) in [
            ("PUBLIC", "public"),
            (" public", "public"),
            ("\"S2 Mixed\"", "\"S2 Mixed\""),
        ] {
            let read = from_text(&schemas, written).map(|datum| printed(&datum));
            assert_eq!(read.ok().as_deref(), Some(expected), "{written}");
        }
    }

    /// **Two refusals for two mistakes**: `42602` for a string that is not one name and `3F000` for
    /// a name no schema has — and `NULL` for both from `to_regnamespace`, measured.
    #[test]
    fn a_malformed_name_and_a_missing_schema_are_two_refusals() {
        let schemas = schemas();
        for malformed in ["a.b", "S2 Mixed", "", "\"left open"] {
            let state = from_text(&schemas, malformed)
                .err()
                .map(|error| error.sqlstate().to_owned());
            assert_eq!(state.as_deref(), Some("42602"), "{malformed}");
            assert!(try_from_text(&schemas, malformed).is_none(), "{malformed}");
        }
        let error = from_text(&schemas, "nosuch").err();
        assert_eq!(
            error.map(|error| error.to_string()).as_deref(),
            Some("schema \"nosuch\" does not exist")
        );
        assert!(try_from_text(&schemas, "nosuch").is_none());
    }

    /// **Digits are an oid**, whatever schema has it: its name when one does, its digits when none
    /// does, and `-` for 0 — and the value layer, with no schemas, reads only the digits.
    #[test]
    fn digits_are_an_oid() {
        let schemas = schemas();
        for (written, expected) in [("41", "s2ns"), ("99999", "99999"), ("-", "-")] {
            let read = from_text(&schemas, written).map(|datum| printed(&datum));
            assert_eq!(read.ok().as_deref(), Some(expected), "{written}");
        }
        assert_eq!(printed(&of_oid(&schemas, 0)), "-");
        assert_eq!(
            without_a_catalog(" 41 ")
                .map(|datum| printed(&datum))
                .ok()
                .as_deref(),
            Some("41")
        );
        let state = without_a_catalog("public")
            .err()
            .map(|error| error.sqlstate().to_owned());
        assert_eq!(state.as_deref(), Some("0A000"));
    }
}
