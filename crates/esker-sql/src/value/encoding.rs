//! PostgreSQL's character-set names, and nothing else about them.
//!
//! One question is asked here and it decides which of two errors a caller raises:
//! `convert_to('A', 'LATIN1')` is a **refusal** — a real encoding this node cannot transcode to —
//! and `convert_to('A', 'NOT_AN_ENCODING')` is `22023`, which is what a real server answers. Told
//! apart by name, because the difference is whether PostgreSQL knows the name at all.
//!
//! The list is `pg_encoding_to_char`'s, read off a running 19beta1 rather than typed from memory.

/// Every encoding PostgreSQL 19beta1 names, **folded**: uppercase, with `-` and `_` removed.
///
/// Folded because that is how `pg_char_to_encoding` matches — `UTF8`, `utf8` and `Utf-8` are one
/// name — so the comparison is done once here rather than at every call site.
const ENCODINGS: [&str; 41] = [
    "BIG5",
    "EUCCN",
    "EUCJIS2004",
    "EUCJP",
    "EUCKR",
    "EUCTW",
    "GB18030",
    "GBK",
    "ISO88595",
    "ISO88596",
    "ISO88597",
    "ISO88598",
    "JOHAB",
    "KOI8R",
    "KOI8U",
    "LATIN1",
    "LATIN10",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "SHIFTJIS2004",
    "SJIS",
    "SQLASCII",
    "UHC",
    "UTF8",
    "WIN1250",
    "WIN1251",
    "WIN1252",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1256",
    "WIN1257",
    "WIN1258",
    "WIN866",
    "WIN874",
];

/// Whether PostgreSQL knows this **folded** name as an encoding.
#[must_use]
pub fn is_postgresql_encoding(folded: &str) -> bool {
    // `UNICODE` is an alias PostgreSQL accepts for `UTF8` and is not in `pg_encoding_to_char`'s
    // output, so it is named here rather than left to fall through to the `22023`.
    folded == "UNICODE" || ENCODINGS.contains(&folded)
}
