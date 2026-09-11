//! PostgreSQL's `"char"` — **one byte**, and not `character(1)`.
//!
//! The catalog's own one-character type: `pg_class.relkind`, `pg_constraint.contype`,
//! `pg_type.typcategory` and `typdelim` are all this. The quotes are part of how it is written,
//! because an unquoted `char` means `bpchar`.
//!
//! **The byte is what is kept, not the character**, which is the whole of this module. `'abc'` is
//! `a`; `'é'` is the *first byte* of a two-byte character and is not valid UTF-8 on its own, so a
//! real server's output function writes it as the octal escape `\303` — measured, and
//! `octet_length` then counts that escape's four characters rather than the one byte behind it.
//! Carrying the escape as the **value** is what makes that true of every reader rather than of one
//! printer.
//!
//! Measured in `tests/captures/pg19_char_type.txt`.

/// The value a text reaches a `"char"` column as.
///
/// The first byte, rendered the way PostgreSQL's `charout` renders it: itself when it is printable
/// ASCII, and a three-digit octal escape when it is not — which is every byte a multi-byte
/// character starts with. An empty input stays empty, which is a legal `"char"` and is not NULL.
///
/// **It is idempotent, and that is PostgreSQL's own behaviour rather than a convenience**:
/// `'\303'::"char"` is `\303` on a real server and not `\` — `charin` reads the escape form back
/// — so `'é'::"char"::"char"` is `\303` too, measured. It matters here because a folded cast keeps
/// its `Cast` node ([ADR 0086](../../../../docs/adr/0086-a-folded-cast-keeps-the-type-it-named.md)),
/// so the evaluator reads the already-rendered value a second time; without this that second pass
/// took the backslash as the byte.
#[must_use]
pub fn of_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    render(to_byte(text))
}

/// One byte as `charout` writes it.
#[must_use]
pub fn render(byte: u8) -> String {
    if byte == 0 {
        return String::new();
    }
    if byte.is_ascii() && !byte.is_ascii_control() {
        return char::from(byte).to_string();
    }
    format!("\\{byte:03o}")
}

/// The byte a `"char"` holds, for the `int4` cast.
///
/// The inverse of [`render`]: measured, `'r'::"char"::int4` is 114 and `65::int4::"char"` is `A`,
/// so the cast is the byte's own value in both directions and not a parse of what it prints.
#[must_use]
pub fn to_byte(text: &str) -> u8 {
    if let Some(octal) = text.strip_prefix('\\')
        && octal.len() == 3
        && let Ok(byte) = u8::from_str_radix(octal, 8)
    {
        return byte;
    }
    text.as_bytes().first().copied().unwrap_or(0)
}

/// The `int4` a `"char"` casts to: its byte, **signed**.
///
/// `'r'::"char"::int4` is 114 and `'\303'::"char"::int4` is **-61**, not 195 — PostgreSQL's
/// `chartoi4` reads the byte as the signed C type, and one measurement is enough to see it where
/// reasoning goes the other way. Both callers of this cast use this function: `parse::lower`'s
/// fold, which knows the literal's declared type, and `exec::cursor`, which has to ask the
/// *plan* for it because a `"char"` and a `text` are the same `Datum::Text`.
#[must_use]
pub fn to_int4(text: &str) -> i32 {
    i32::from(i8::from_ne_bytes([to_byte(text)]))
}

#[cfg(test)]
mod tests {
    use super::{of_text, render, to_byte};

    #[test]
    fn reading_an_escape_back_gives_the_byte_it_names() {
        // Measured: `'\303'::"char"` is `\303` on a real server, so the function is idempotent —
        // and the folded cast's second pass depends on it.
        assert_eq!(of_text("\\303"), "\\303");
        assert_eq!(of_text(&of_text("é")), of_text("é"));
    }

    #[test]
    fn a_multi_byte_character_keeps_its_first_byte_as_an_escape() {
        // `é` is `\303\251`; the first byte survives and prints as an octal escape, which is what
        // makes `octet_length('é'::"char")` four on a real server.
        assert_eq!(of_text("é"), "\\303");
        assert_eq!(of_text("abc"), "a");
        assert_eq!(of_text(""), "");
    }

    #[test]
    fn the_byte_survives_a_round_trip_through_the_escape() {
        for byte in 1..=255u8 {
            assert_eq!(to_byte(&render(byte)), byte, "byte {byte}");
        }
        assert_eq!(to_byte("r"), 114);
        assert_eq!(render(65), "A");
    }
}
