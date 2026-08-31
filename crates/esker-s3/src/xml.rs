//! Reading the two XML documents S3 replies with, without an XML parser.
//!
//! `ListObjectsV2` is the only call whose *body* we have to understand, and what we need from
//! it is three fields per object plus a continuation token. That does not justify a dependency,
//! and `CLAUDE.md` would not allow one anyway.
//!
//! So this is a scanner, not a parser. It finds `<Tag>` … `</Tag>` pairs at the depth it
//! expects and ignores everything else, which means it is **wrong** on documents S3 does not
//! send — nested `<Key>` elements, CDATA, comments containing a tag name — and correct on the
//! ones it does. That trade is stated rather than hidden, and the failure mode is a listing
//! that is short or a key that is odd, never a panic and never an unbounded loop.
//!
//! Everything below is total: no indexing that can go out of range, no recursion, and a
//! `while` whose cursor strictly advances.

/// The contents of the first `<tag>…</tag>` at or after `from`, and where it ended.
///
/// Returns `None` when the tag does not appear, or appears without a closing partner.
pub(crate) fn element<'a>(xml: &'a str, tag: &str, from: usize) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let rest = xml.get(from..)?;
    let start = rest.find(&open)? + open.len();
    let end = rest.get(start..)?.find(&close)? + start;
    Some((rest.get(start..end)?, from + end + close.len()))
}

/// The five named entities XML defines, plus numeric character references.
///
/// The numeric half is not theoretical: **`MinIO` writes an `ETag` as `&#34;abc&#34;`**, where S3
/// writes `&quot;abc&quot;`. A scanner that handles only the named entities returns an `ETag`
/// with `&#34;` still in it, which then never compares equal to the one the `PutObject` response
/// header carried — and the ranged-read check in
/// [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 6 silently starts
/// failing every read. Found by the container test, not by reasoning about it.
///
/// A reference that is malformed, out of range, or not a Unicode scalar value is left exactly
/// as it was written. Showing `&#xD800;` is better than inventing a character for it.
pub(crate) fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let named = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // `&amp;` last: doing it first would turn `&amp;lt;` into `<`.
        .replace("&amp;", "&");
    numeric_references(&named)
}

/// Decodes `&#NN;` and `&#xHH;`, leaving anything else untouched.
fn numeric_references(text: &str) -> String {
    if !text.contains("&#") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find(';') else {
            // No terminator: the rest of the string is not a reference.
            out.push_str(&rest[at..]);
            return out;
        };
        let digits = &after[..end];
        let decoded = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            digits.parse::<u32>().ok()
        }
        .and_then(char::from_u32);

        match decoded {
            Some(ch) => out.push(ch),
            // Malformed or unrepresentable: keep the source text, do not guess.
            None => out.push_str(&rest[at..at + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// An `ETag` as the client stores it: the server's value with its surrounding quotes removed.
///
/// S3 sends `"abc"` in a header and `&quot;abc&quot;` in a listing. Both mean the same tag, and
/// storing one form means comparing like with like later.
pub(crate) fn normalise_etag(raw: &str) -> String {
    unescape(raw.trim()).trim_matches('"').to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{element, normalise_etag, unescape};

    #[test]
    fn an_element_is_found_and_the_cursor_advances() {
        let xml = "<a><Key>one</Key><Key>two</Key></a>";
        let (first, after) = element(xml, "Key", 0).unwrap();
        assert_eq!(first, "one");
        let (second, after2) = element(xml, "Key", after).unwrap();
        assert_eq!(second, "two");
        assert!(after2 > after);
        assert!(element(xml, "Key", after2).is_none());
    }

    #[test]
    fn a_missing_or_unclosed_tag_is_none_not_a_panic() {
        assert!(element("<a></a>", "Key", 0).is_none());
        assert!(element("<Key>dangling", "Key", 0).is_none());
        assert!(element("", "Key", 0).is_none());
        // A `from` past the end must not index out of range.
        assert!(element("<Key>x</Key>", "Key", 9_999).is_none());
    }

    /// A key containing a multi-byte character means `from` can land mid-character if the
    /// arithmetic is wrong. `str::get` returning `None` is the guard, and this is the test that
    /// exercises it.
    #[test]
    fn multibyte_keys_do_not_split_a_character() {
        let xml = "<Key>héllo</Key><Key>wörld</Key>";
        let (first, after) = element(xml, "Key", 0).unwrap();
        assert_eq!(first, "héllo");
        assert_eq!(element(xml, "Key", after).unwrap().0, "wörld");
    }

    #[test]
    fn entities_decode_and_ampersand_goes_last() {
        assert_eq!(unescape("a&amp;b"), "a&b");
        assert_eq!(unescape("&quot;x&quot;"), "\"x\"");
        assert_eq!(unescape("&amp;lt;"), "&lt;");
        assert_eq!(unescape("plain"), "plain");
    }

    /// `MinIO` writes `&#34;` where S3 writes `&quot;`. The container test found this by
    /// comparing an `ETag` from a listing against the same `ETag` from a `PutObject` response
    /// header and getting two different strings.
    #[test]
    fn numeric_character_references_decode() {
        assert_eq!(unescape("&#34;abc&#34;"), "\"abc\"");
        assert_eq!(unescape("&#x22;abc&#x22;"), "\"abc\"");
        assert_eq!(unescape("&#65;&#66;"), "AB");
        assert_eq!(unescape("a&#233;b"), "aéb");
        assert_eq!(
            normalise_etag("&#34;6c6cec3742614469c72c241ccd45bebc&#34;"),
            "6c6cec3742614469c72c241ccd45bebc",
            "the exact value MinIO returned when this bug was found"
        );
    }

    /// A reference that is not one is shown, not guessed at. None of these may panic.
    #[test]
    fn malformed_references_are_left_alone() {
        assert_eq!(unescape("&#;"), "&#;");
        assert_eq!(unescape("&#banana;"), "&#banana;");
        assert_eq!(unescape("&#99999999999;"), "&#99999999999;");
        assert_eq!(
            unescape("&#xD800;"),
            "&#xD800;",
            "a surrogate is not a char"
        );
        assert_eq!(unescape("&#34"), "&#34", "no terminator");
        assert_eq!(unescape("a&#34;b&#"), "a\"b&#");
        assert_eq!(unescape("&"), "&");
    }

    #[test]
    fn etags_normalise_to_the_same_string_from_a_header_or_a_listing() {
        assert_eq!(normalise_etag("\"abc\""), "abc");
        assert_eq!(normalise_etag("&quot;abc&quot;"), "abc");
        assert_eq!(normalise_etag("  \"abc\"  "), "abc");
        assert_eq!(normalise_etag("abc"), "abc");
    }
}
