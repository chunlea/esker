//! TLS on the PostgreSQL port, and the only module in this crate that knows what TLS is.
//!
//! The rule `sqlparser` lives under (ADR 0014) applies here too: a `use rustls::` anywhere else in
//! this crate is a review failure. Everything above this module sees [`TlsConfig`], which exists in
//! both builds, and a byte stream.
//!
//! # Where the session driver lives
//!
//! `esker_proto::transport::tls`, not here. The RPC surface needed the same thing — a `tokio`
//! socket, a sans-io state machine, and something to carry bytes between them — and one driver
//! for both is the point of ADR 0055's third unit. This module keeps what is specific to *this*
//! surface: the `SSLRequest` answer, which PEM label maps to which key shape, and a `TlsConfig`
//! that exists in both builds so `Config` needs no `cfg`.
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
//! # Where the PEM reading happens
//!
//! `esker_base::pem`, which is neither TLS nor a dependency's job: base64 between two labelled
//! lines, naming no cryptographic type. `rustls-pemfile` would have been a third crate against an
//! exception that names two, and the reader had grown three copies before it was moved down.
//! What stays here is the part that *is* TLS: which label maps to which `PrivateKeyDer` shape.

use std::fmt;
use std::path::{Path, PathBuf};

pub use esker_proto::transport::MaybeTlsStream;

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

    /// The configuration a handshake runs against, if there is one.
    #[cfg(feature = "tls")]
    pub(crate) fn server(&self) -> Option<&std::sync::Arc<rustls::ServerConfig>> {
        self.server.as_ref()
    }

    /// Reads a PEM certificate chain and private key and builds a server configuration from them.
    ///
    /// # Errors
    ///
    /// [`TlsError::NotCompiledIn`] when the `tls` feature is off — never a silently disabled
    /// configuration. Otherwise: the file could not be read, held no PEM block of the right kind,
    /// was not valid base64, or `rustls` rejected the pair.
    #[cfg_attr(
        not(feature = "tls"),
        expect(unused_variables, reason = "no TLS to configure")
    )]
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

            let chain: Vec<CertificateDer<'static>> = read_pem(certificate, "CERTIFICATE")?
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
            let key_der = if let Some(der) = read_first(key, "PRIVATE KEY")? {
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der))
            } else if let Some(der) = read_first(key, "EC PRIVATE KEY")? {
                PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der))
            } else if let Some(der) = read_first(key, "RSA PRIVATE KEY")? {
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
/// Every PEM block of `label` in `path`, read and decoded.
fn read_pem(path: &Path, label: &'static str) -> Result<Vec<Vec<u8>>, TlsError> {
    let text = std::fs::read_to_string(path).map_err(|source| TlsError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;
    esker_base::pem::blocks(&text, label).map_err(|reason| TlsError::Malformed {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    })
}

#[cfg(feature = "tls")]
/// The first PEM block of `label` in `path`, or `None`.
fn read_first(path: &Path, label: &'static str) -> Result<Option<Vec<u8>>, TlsError> {
    Ok(read_pem(path, label)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal that is the whole point of unit 2, in the build that has no TLS.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn configuring_tls_without_the_feature_is_an_error() {
        let Err(error) =
            TlsConfig::from_pem_files(Path::new("/nonexistent.pem"), Path::new("/k.pem"))
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
