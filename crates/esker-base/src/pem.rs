//! PEM: base64 between two labelled lines, decoded to the DER inside.
//!
//! Here rather than in a TLS crate because it is neither TLS nor a dependency's job: it is base64
//! and a pair of markers, it names no cryptographic type, and it is the same in every caller.
//! Three copies of it existed before this module — one in the PostgreSQL port, one in the S3
//! client, one in a test — which is how a strictness decision made in one place quietly fails to
//! be made in another.
//!
//! # It never panics, and it never guesses
//!
//! A certificate file is bytes this process did not write (`CLAUDE.md` invariant 9), so every
//! length and every byte is checked and every failure is a value. And the padding is **required**:
//! unpadded base64 is a real encoding elsewhere, PEM always pads, and a file that does not is
//! malformed rather than an invitation to work out what it probably meant — the same rule
//! `esker-s3`'s HTTP response parser follows for a header it does not recognise.

/// What was wrong with the text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PemError {
    /// A `-----BEGIN x-----` with no matching `-----END x-----`.
    #[error("a {label} block is never closed")]
    Unterminated {
        /// The label that opened and did not close.
        label: String,
    },
    /// A byte that is not in the base64 alphabet, outside the padding.
    #[error("{0:?} is not a base64 character")]
    NotBase64(char),
    /// More than two `=`, or data after them.
    #[error("{0}")]
    Padding(&'static str),
    /// A final quantum that no input could have produced.
    #[error("{0}")]
    Truncated(&'static str),
}

/// Every block in `text` carrying `label`, decoded.
///
/// Text outside a block is ignored, which is what lets a certificate file carry the human-readable
/// summary `openssl` writes above it. An empty result is not an error — the caller knows whether
/// finding nothing is one, and says so in its own words with the file's name attached.
///
/// # Errors
///
/// A block that opens and never closes, or whose payload is not canonical base64.
pub fn blocks(text: &str, label: &str) -> Result<Vec<Vec<u8>>, PemError> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else {
            return Err(PemError::Unterminated {
                label: label.to_owned(),
            });
        };
        blocks.push(base64(&after[..stop])?);
        rest = &after[stop + end.len()..];
    }
    Ok(blocks)
}

/// The first block carrying `label`, if there is one.
///
/// # Errors
///
/// As [`blocks`].
pub fn first_block(text: &str, label: &str) -> Result<Option<Vec<u8>>, PemError> {
    Ok(blocks(text, label)?.into_iter().next())
}

/// Decodes standard base64, ignoring the ASCII whitespace PEM wraps its payload with.
///
/// # Errors
///
/// A byte outside the alphabet, padding that runs past two characters or has data after it, or a
/// final group that no input could have produced.
pub fn base64(text: &str) -> Result<Vec<u8>, PemError> {
    /// Position in the base64 alphabet, or `None` for a byte outside it.
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    // A quantum is four encoded characters; `pad` counts the `=`, which may appear only at the
    // very end and only once or twice.
    let mut quantum = 0u32;
    let mut filled = 0;
    let mut pad = 0;
    for byte in text.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            pad += 1;
            if pad > 2 {
                return Err(PemError::Padding("base64 padding runs past two characters"));
            }
            continue;
        }
        if pad > 0 {
            return Err(PemError::Padding("base64 data after the padding"));
        }
        let Some(value) = value(byte) else {
            return Err(PemError::NotBase64(byte as char));
        };
        quantum = (quantum << 6) | value;
        filled += 1;
        if filled == 4 {
            // The quantum holds exactly 24 bits, so the low three bytes of its big-endian form
            // *are* the output: no cast, no mask, nothing to get wrong.
            let [_, first, second, third] = quantum.to_be_bytes();
            out.extend_from_slice(&[first, second, third]);
            quantum = 0;
            filled = 0;
        }
    }
    // What is left over has to agree with the padding, exactly.
    match (filled, pad) {
        (0, 0) => Ok(out),
        (3, 1) => {
            let [_, first, second, _] = (quantum << 6).to_be_bytes();
            out.extend_from_slice(&[first, second]);
            Ok(out)
        }
        (2, 2) => {
            let [_, only, _, _] = (quantum << 12).to_be_bytes();
            out.push(only);
            Ok(out)
        }
        (1, _) => Err(PemError::Truncated(
            "a base64 group has one character left over, which encodes nothing",
        )),
        _ => Err(PemError::Truncated(
            "base64 input ends mid-group, or its padding is missing",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{PemError, base64, blocks, first_block};

    /// The vectors from RFC 4648 §10, which is where this alphabet is specified.
    #[test]
    fn decodes_the_rfc_4648_vectors() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(
                base64(encoded).as_deref(),
                Ok(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    /// PEM wraps at 64 columns, so newlines fall anywhere.
    #[test]
    fn ignores_the_whitespace_pem_wraps_with() {
        assert_eq!(base64("Zm9v\nYmFy\r\n").as_deref(), Ok(&b"foobar"[..]));
        assert_eq!(base64(" Z m 9 v ").as_deref(), Ok(&b"foo"[..]));
    }

    /// Invariant 9, on the bytes an operator's file actually contains.
    #[test]
    fn malformed_base64_is_an_error_not_a_panic() {
        assert_eq!(base64("Zm9vYmFy!"), Err(PemError::NotBase64('!')));
        assert!(matches!(base64("Z"), Err(PemError::Truncated(_))));
        assert!(matches!(base64("Zg==="), Err(PemError::Padding(_))));
        assert!(matches!(base64("Zg==Zg=="), Err(PemError::Padding(_))));
        // Unpadded input is refused rather than guessed at: PEM always pads.
        assert!(matches!(base64("Zm9vYmF"), Err(PemError::Truncated(_))));
        // A BOM is not whitespace.
        assert!(base64("\u{feff}Zm8").is_err());
    }

    #[test]
    fn reads_the_blocks_it_is_asked_for_and_ignores_the_rest() {
        let text = "\
subject=CN = localhost
-----BEGIN CERTIFICATE-----
Zm9vYmFy
-----END CERTIFICATE-----
-----BEGIN PRIVATE KEY-----
Zm9v
-----END PRIVATE KEY-----
-----BEGIN CERTIFICATE-----
Zm8=
-----END CERTIFICATE-----
";
        assert_eq!(
            blocks(text, "CERTIFICATE"),
            Ok(vec![b"foobar".to_vec(), b"fo".to_vec()])
        );
        assert_eq!(first_block(text, "PRIVATE KEY"), Ok(Some(b"foo".to_vec())));
        assert_eq!(blocks(text, "EC PRIVATE KEY"), Ok(Vec::new()));
        assert_eq!(first_block(text, "EC PRIVATE KEY"), Ok(None));
    }

    #[test]
    fn an_unclosed_block_is_an_error() {
        assert_eq!(
            blocks("-----BEGIN CERTIFICATE-----\nZm9v\n", "CERTIFICATE"),
            Err(PemError::Unterminated {
                label: "CERTIFICATE".to_owned()
            })
        );
    }
}
