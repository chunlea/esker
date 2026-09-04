//! TLS on the PostgreSQL port, and the only module in this crate that knows what TLS is.
//!
//! The rule `sqlparser` lives under (ADR 0014) applies here too: a `use rustls::` anywhere else in
//! this crate is a review failure. Everything above this module sees [`TlsConfig`], which exists in
//! both builds, and a byte stream.
//!
//! # Two builds, one API
//!
//! `rustls` and `rustls-graviola` are optional dependencies behind the crate's `tls` feature, which
//! is **off by default** ([ADR 0055](../../../../docs/adr/0055-the-tls-options-across-three-surfaces-measured.md),
//! accepted 2026-09-04; the provider is graviola because it is the only pure-Rust `CryptoProvider`
//! that passes `cargo deny check` against this repo's own policy). [`TlsConfig`] is present either
//! way, and [`TlsConfig::from_pem_files`] is the seam: with the feature off it returns
//! [`TlsError::NotCompiledIn`] rather than quietly leaving TLS unconfigured.
//!
//! **That refusal is the point.** A node told `--tls-cert` by an operator who believes the port is
//! encrypted, which then serves plaintext because the binary was built without the feature, is the
//! worst outcome available — the same argument `esker_s3::Endpoint::parse` makes when it refuses
//! `https://` instead of downgrading it (ADR 0025).
//!
//! # Why the PEM reader is in here
//!
//! `rustls-pemfile` is a crate, and the exception the maintainer granted names two: rustls and its
//! provider. PEM is a base64 payload between two labelled lines; that is a hundred lines with tests
//! and it is the kind of thing `CLAUDE.md` says to write. It never panics on input — every length
//! and every byte is checked, because a certificate file is bytes this process did not write
//! (invariant 9).

use std::fmt;
use std::path::{Path, PathBuf};

/// What went wrong configuring TLS.
///
/// These are all *startup* failures: the node refuses to come up rather than coming up without the
/// encryption it was told to provide.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// `--tls-cert`/`--tls-key` were given to a binary built without the `tls` feature.
    #[error(
        "TLS was configured ({path}) but this binary was built without it: rebuild with \
         `--features tls`, or remove --tls-cert/--tls-key and terminate TLS in front of this node \
         (docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)"
    )]
    NotCompiledIn {
        /// The file the operator named, so the message says which flag was believed.
        path: PathBuf,
    },
    /// The certificate or key file could not be read.
    #[error("reading {path}: {source}")]
    Unreadable {
        /// The file that could not be read.
        path: PathBuf,
        /// Why not.
        source: std::io::Error,
    },
    /// The file was read but holds no PEM block of the expected kind.
    #[error("{path} contains no {label} block")]
    NoPemBlock {
        /// The file that was parsed.
        path: PathBuf,
        /// The label that was looked for, e.g. `CERTIFICATE`.
        label: &'static str,
    },
    /// A PEM block's base64 payload is not base64.
    #[error("{path}: {reason}")]
    Malformed {
        /// The file that was parsed.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
    /// `rustls` refused the certificate and key.
    #[error("rustls rejected the certificate or key: {0}")]
    Rejected(String),
}

/// What the listener was told about TLS.
///
/// Cloned per connection, so the expensive part — the parsed certificate chain and the
/// `rustls::ServerConfig` built from it — lives behind an `Arc` and is shared.
#[derive(Clone, Default)]
pub struct TlsConfig {
    #[cfg(feature = "tls")]
    server: Option<std::sync::Arc<rustls::ServerConfig>>,
}

impl fmt::Debug for TlsConfig {
    /// Deliberately opaque. A `ServerConfig` holds key material, and a `Config` derives `Debug`,
    /// which is how a private key ends up in a log line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsConfig")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl TlsConfig {
    /// A node that terminates no TLS. The default, and what every existing caller gets.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Whether this node can answer `S` to an `SSLRequest`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            self.server.is_some()
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }

    /// Reads a PEM certificate chain and private key and builds a server configuration from them.
    ///
    /// # Errors
    ///
    /// [`TlsError::NotCompiledIn`] when the `tls` feature is off — never a silently disabled
    /// configuration. Otherwise: the file could not be read, held no PEM block of the right kind,
    /// was not valid base64, or `rustls` rejected the pair.
    #[cfg_attr(not(feature = "tls"), expect(unused_variables, reason = "no TLS to configure"))]
    pub fn from_pem_files(certificate: &Path, key: &Path) -> Result<Self, TlsError> {
        #[cfg(not(feature = "tls"))]
        {
            Err(TlsError::NotCompiledIn {
                path: certificate.to_path_buf(),
            })
        }
        #[cfg(feature = "tls")]
        {
            use rustls::pki_types::{
                CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer,
                PrivateSec1KeyDer,
            };

            let chain: Vec<CertificateDer<'static>> = pem_blocks(certificate, "CERTIFICATE")?
                .into_iter()
                .map(CertificateDer::from)
                .collect();
            // `with_single_cert` is documented to take a non-empty chain, and an empty one is what
            // an operator gets from a file of the wrong kind. Checked here so the message names the
            // file rather than coming out of rustls without one.
            if chain.is_empty() {
                return Err(TlsError::NoPemBlock {
                    path: certificate.to_path_buf(),
                    label: "CERTIFICATE",
                });
            }

            // The three labels a PEM private key comes under, in the order a generated key is most
            // likely to carry. Each maps to the DER shape rustls names for it; guessing wrong here
            // is an error, never an attempt to parse it as something else.
            let key_der = if let Some(der) = first_pem_block(key, "PRIVATE KEY")? {
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der))
            } else if let Some(der) = first_pem_block(key, "EC PRIVATE KEY")? {
                PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der))
            } else if let Some(der) = first_pem_block(key, "RSA PRIVATE KEY")? {
                PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der))
            } else {
                return Err(TlsError::NoPemBlock {
                    path: key.to_path_buf(),
                    label: "PRIVATE KEY",
                });
            };

            // The provider is passed rather than installed: `install_default` is process-global
            // state, and a library that sets it decides for every other user of rustls in the
            // process, including a test that wanted a different one.
            let provider = std::sync::Arc::new(rustls_graviola::default_provider());
            let server = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|error| TlsError::Rejected(error.to_string()))?
                .with_no_client_auth()
                .with_single_cert(chain, key_der)
                .map_err(|error| TlsError::Rejected(error.to_string()))?;
            Ok(Self {
                server: Some(std::sync::Arc::new(server)),
            })
        }
    }

}

#[cfg(feature = "tls")]
/// Every PEM block in `path` carrying `label`, base64-decoded.
fn pem_blocks(path: &Path, label: &'static str) -> Result<Vec<Vec<u8>>, TlsError> {
    let text = std::fs::read_to_string(path).map_err(|source| TlsError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;
    decode_pem(&text, label).map_err(|reason| TlsError::Malformed {
        path: path.to_path_buf(),
        reason,
    })
}

#[cfg(feature = "tls")]
/// The first PEM block carrying `label`, or `None` if the file has none.
fn first_pem_block(path: &Path, label: &'static str) -> Result<Option<Vec<u8>>, TlsError> {
    Ok(pem_blocks(path, label)?.into_iter().next())
}

#[cfg(any(feature = "tls", test))]
/// Pulls every `-----BEGIN <label>-----` … `-----END <label>-----` block out of `text`.
///
/// Anything outside a block is ignored, which is what lets a certificate file carry the human
/// -readable summary `openssl` writes above the block. A block that opens and never closes, or
/// whose payload is not base64, is an error rather than a shorter certificate.
fn decode_pem(text: &str, label: &str) -> Result<Vec<Vec<u8>>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else {
            return Err(format!("a {label} block is never closed"));
        };
        blocks.push(base64_decode(&after[..stop])?);
        rest = &after[stop + end.len()..];
    }
    Ok(blocks)
}

#[cfg(any(feature = "tls", test))]
/// Decodes standard base64, ignoring ASCII whitespace, which is how PEM wraps its payload.
///
/// Written here rather than taken from a crate, for the reason the module doc gives. It never
/// panics: every index is checked and every byte outside the alphabet is an error.
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    /// Position in the base64 alphabet, or `None` for a byte that is not in it.
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
    // A quantum is four encoded characters; `pad` counts the `=` seen, which may only appear at
    // the very end and only one or two of them.
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
                return Err("base64 padding runs past two characters".to_owned());
            }
            continue;
        }
        if pad > 0 {
            return Err("base64 data after the padding".to_owned());
        }
        let Some(value) = value(byte) else {
            return Err(format!("{:?} is not a base64 character", byte as char));
        };
        quantum = (quantum << 6) | value;
        filled += 1;
        if filled == 4 {
            out.extend_from_slice(&[
                u8::try_from((quantum >> 16) & 0xff).unwrap_or_default(),
                u8::try_from((quantum >> 8) & 0xff).unwrap_or_default(),
                u8::try_from(quantum & 0xff).unwrap_or_default(),
            ]);
            quantum = 0;
            filled = 0;
        }
    }
    // What is left over has to agree with the padding, **exactly**: three characters are two bytes
    // and need one `=`, two are one byte and need two, and one on its own cannot have come from
    // any input. Unpadded base64 is a real encoding elsewhere and is refused here, because PEM
    // always pads and a key file that does not is malformed — the same "reject rather than guess"
    // the HTTP response parser follows (ADR 0025) and for the same reason: the alternative is
    // deciding on a user's behalf what their key file probably meant.
    match (filled, pad) {
        (0, 0) => Ok(out),
        (3, 1) => {
            let bits = quantum << 6;
            out.extend_from_slice(&[
                u8::try_from((bits >> 16) & 0xff).unwrap_or_default(),
                u8::try_from((bits >> 8) & 0xff).unwrap_or_default(),
            ]);
            Ok(out)
        }
        (2, 2) => {
            let bits = quantum << 12;
            out.push(u8::try_from((bits >> 16) & 0xff).unwrap_or_default());
            Ok(out)
        }
        (1, _) => Err("a base64 quantum has one character left over, which encodes nothing".into()),
        _ => Err("base64 input ends mid-quantum, or its padding is missing".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                base64_decode(encoded).as_deref(),
                Ok(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    /// PEM wraps at 64 columns, so the decoder has to ignore newlines wherever they fall.
    #[test]
    fn ignores_the_whitespace_pem_wraps_with() {
        assert_eq!(base64_decode("Zm9v\nYmFy\r\n").as_deref(), Ok(&b"foobar"[..]));
        assert_eq!(base64_decode(" Z m 9 v ").as_deref(), Ok(&b"foo"[..]));
    }

    /// Invariant 9: a certificate file is bytes this process did not write.
    #[test]
    fn malformed_base64_is_an_error_not_a_panic() {
        for bad in [
            "Zm9vYmFy!",   // not in the alphabet
            "Z",           // one character left over
            "Zg===",       // three pad characters
            "Zg==Zg==",    // data after the padding
            "Zm9vYmF",     // ends mid-quantum with no padding
            "\u{feff}Zm8", // a BOM is not whitespace
        ] {
            assert!(base64_decode(bad).is_err(), "{bad:?} decoded successfully");
        }
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
            decode_pem(text, "CERTIFICATE"),
            Ok(vec![b"foobar".to_vec(), b"fo".to_vec()])
        );
        assert_eq!(decode_pem(text, "PRIVATE KEY"), Ok(vec![b"foo".to_vec()]));
        assert_eq!(decode_pem(text, "EC PRIVATE KEY"), Ok(Vec::new()));
    }

    #[test]
    fn an_unclosed_block_is_an_error() {
        let text = "-----BEGIN CERTIFICATE-----\nZm9v\n";
        assert!(decode_pem(text, "CERTIFICATE").is_err());
    }

    /// The refusal that is the whole point of unit 2, in the build that has no TLS.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn configuring_tls_without_the_feature_is_an_error() {
        let Err(error) = TlsConfig::from_pem_files(Path::new("/nonexistent.pem"), Path::new("/k.pem"))
        else {
            panic!("a build without the feature must refuse to configure TLS");
        };
        assert!(matches!(error, TlsError::NotCompiledIn { .. }));
        // The operator has to be able to act on it: the message names the way out.
        assert!(error.to_string().contains("--features tls"));
    }

    #[test]
    fn a_disabled_config_says_so_and_keeps_its_key_material_out_of_debug() {
        let config = TlsConfig::disabled();
        assert!(!config.is_enabled());
        assert_eq!(format!("{config:?}"), "TlsConfig { enabled: false }");
    }
}
