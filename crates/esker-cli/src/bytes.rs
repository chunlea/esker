//! Rendering arbitrary bytes for a terminal.
//!
//! Every dump in this crate prints keys and values, and a key is opaque bytes (`CLAUDE.md`
//! invariant 7). Not UTF-8, not text, not anything: a tool that assumed otherwise would either
//! panic on a real key or lie about it. Printable ASCII prints as itself and everything else
//! as `\xNN`, which round-trips by eye and never surprises a terminal.

/// Renders `bytes` with non-printable characters escaped.
pub(crate) fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(byte as char),
            _ => {
                const HEX: [u8; 16] = *b"0123456789abcdef";
                out.push('\\');
                out.push('x');
                out.push(HEX[usize::from(byte >> 4)] as char);
                out.push(HEX[usize::from(byte & 0x0f)] as char);
            }
        }
    }
    out
}

/// [`escape`], truncated for a summary field, with the full length still named.
pub(crate) fn escape_capped(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        return escape(bytes);
    }
    format!("{}... ({} bytes)", escape(&bytes[..limit]), bytes.len())
}

/// `"file"` or `"files"`, so a report does not say "1 files".
pub(crate) fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, escape_capped, plural};

    #[test]
    fn escaping_assumes_nothing_about_a_key() {
        assert_eq!(escape(b"plain"), "plain");
        assert_eq!(escape(b""), "");
        assert_eq!(escape(b"\x00\xff\n"), "\\x00\\xff\\x0a");
        assert_eq!(escape(b"quote\"back\\slash"), "quote\\\"back\\\\slash");
        // Every byte renders, and none of them panics. 95 printable characters, of which
        // `\` and `"` take two each, and 161 escapes of four.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(escape(&all).len(), (95 + 2) + 161 * 4);
    }

    #[test]
    fn counts_read_as_english() {
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
    }

    #[test]
    fn capping_names_the_length_it_hid() {
        assert_eq!(escape_capped(b"short", 48), "short");
        let long = vec![b'x'; 60];
        let capped = escape_capped(&long, 8);
        assert!(capped.starts_with("xxxxxxxx..."), "{capped}");
        assert!(capped.contains("60 bytes"), "{capped}");
    }
}
