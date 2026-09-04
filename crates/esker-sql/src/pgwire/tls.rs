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

/// A client connection, before or after it became a TLS one.
///
/// [`Connection`](super::server::Connection) is generic over its stream and does not care which of
/// these it has; this exists so the accept path can decide *after* reading the client's
/// `SSLRequest` and still hand one type onwards.
pub enum MaybeTlsStream<S> {
    /// The socket as it arrived.
    Plain(S),
    /// The plaintext side of a TLS session. The ciphertext side is owned by the task
    /// [`accept`] spawned, which is what talks to the socket from then on.
    #[cfg(feature = "tls")]
    Tls(tokio::io::DuplexStream),
}

impl<S> fmt::Debug for MaybeTlsStream<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Plain(_) => "MaybeTlsStream::Plain",
            #[cfg(feature = "tls")]
            Self::Tls(_) => "MaybeTlsStream::Tls",
        })
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for MaybeTlsStream<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for MaybeTlsStream<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(context, bytes),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_write(context, bytes),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(context),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
        }
    }
}

/// How much plaintext may be in flight in each direction between the session and the TLS task.
///
/// One buffer per direction per connection, so it is not free; 64 KiB is four of this protocol's
/// largest ordinary replies and small next to `MAX_MESSAGE_LEN`.
#[cfg(feature = "tls")]
const PLAINTEXT_BUFFER: usize = 64 * 1024;

/// Runs the server handshake on `socket` and returns the plaintext side of the session.
///
/// The caller has already answered `S` to the client's `SSLRequest`; from here the whole
/// connection, the client's real startup packet included, is inside TLS records.
///
/// # The shape of this, and why it is not an `AsyncRead` wrapper
///
/// `rustls` is sans-io: bytes go in, bytes come out, and something has to carry them between the
/// socket and the session. The usual answer is a stream adapter implementing `poll_read`/
/// `poll_write` over the rustls state machine — that is what `tokio-rustls` is, and it is a crate
/// this project has not taken (ADR 0055 accepted two crates, not three).
///
/// Writing that adapter by hand means hand-written `Poll` code where a missed wakeup is a hung
/// connection that reproduces once a week. This does the same job with a task and a duplex pipe:
/// ordinary `async` code, cancel-safe `select!` arms, no manual `Poll` at all. It costs one task
/// and one copy per direction, which this protocol — small requests, small replies, a blocking
/// executor between them — will not notice.
///
/// **The known limit**: the pump reads from the socket only while the session is keeping up, so a
/// peer that sends a large body while refusing to read its reply can stall the connection rather
/// than being backpressured into an error. Request/response traffic cannot reach that state, and
/// `COPY` in both directions at once is not something this node does yet. It is written down here
/// because it is the thing to look at first if a connection ever hangs with data pending.
///
/// # Errors
///
/// Anything the socket does, and any TLS failure: a malformed `ClientHello`, a version or suite with
/// no overlap, a client that goes away mid-handshake. All of them are error values (invariant 9) —
/// a handshake failure ends one connection and touches nothing else.
#[cfg(feature = "tls")]
pub(crate) async fn accept<S>(
    mut socket: S,
    config: &std::sync::Arc<rustls::ServerConfig>,
) -> std::io::Result<tokio::io::DuplexStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    let mut session =
        rustls::ServerConnection::new(std::sync::Arc::clone(config)).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;

    let mut buffer = vec![0u8; 8 * 1024];
    while session.is_handshaking() {
        flush_tls(&mut session, &mut socket).await?;
        if !session.is_handshaking() {
            break;
        }
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the client went away during the TLS handshake",
            ));
        }
        feed_tls(&mut session, &buffer[..read])?;
    }
    // The last flight of the handshake is still queued at this point.
    flush_tls(&mut session, &mut socket).await?;

    let (session_side, caller_side) = tokio::io::duplex(PLAINTEXT_BUFFER);
    tokio::spawn(async move {
        if let Err(error) = pump(socket, session, session_side).await {
            tracing::debug!(%error, "the TLS session ended");
        }
    });
    Ok(caller_side)
}

/// Writes whatever the session has queued to the socket.
#[cfg(feature = "tls")]
async fn flush_tls<S>(session: &mut rustls::ServerConnection, socket: &mut S) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    while session.wants_write() {
        let mut encrypted = Vec::new();
        // Writing into a `Vec` cannot fail, so this only reports a session that has nothing to say.
        if session.write_tls(&mut encrypted)? == 0 {
            break;
        }
        socket.write_all(&encrypted).await?;
    }
    socket.flush().await
}

/// Feeds ciphertext to the session and advances its state machine.
#[cfg(feature = "tls")]
fn feed_tls(session: &mut rustls::ServerConnection, mut ciphertext: &[u8]) -> std::io::Result<()> {
    while !ciphertext.is_empty() {
        // `read_tls` takes what fits in the session's buffer, which may be less than is offered,
        // so this loops rather than assuming one call drains it.
        let taken = session.read_tls(&mut ciphertext)?;
        if taken == 0 {
            break;
        }
        session
            .process_new_packets()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    }
    Ok(())
}

/// Moves everything the session has already decrypted up to the protocol loop.
///
/// Answers `false` when the peer has sent `close_notify`, which is an orderly end rather than a
/// failure. `reader()` reports an empty buffer as `WouldBlock` and a closed one as zero, which is
/// the distinction this turns into a `bool`.
#[cfg(feature = "tls")]
async fn deliver<W>(
    session: &mut rustls::ServerConnection,
    plaintext: &mut W,
    buffer: &mut [u8],
) -> std::io::Result<bool>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    loop {
        match std::io::Read::read(&mut session.reader(), buffer) {
            Ok(0) => return Ok(false),
            Ok(read) => plaintext.write_all(&buffer[..read]).await?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) => return Err(error),
        }
    }
}

/// Carries bytes between the socket and the session for the life of the connection.
#[cfg(feature = "tls")]
async fn pump<S>(
    socket: S,
    mut session: rustls::ServerConnection,
    plaintext: tokio::io::DuplexStream,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    let (mut socket_rx, mut socket_tx) = tokio::io::split(socket);
    let (mut session_rx, mut session_tx) = tokio::io::split(plaintext);
    let mut from_socket = vec![0u8; 8 * 1024];
    let mut from_session = vec![0u8; 8 * 1024];
    let mut decrypted = vec![0u8; 8 * 1024];

    loop {
        // **Deliver first, and before waiting on the socket.** The handshake reads whole TCP
        // segments, and a client is entitled to put its `Finished` and its first application data
        // — here, the PostgreSQL startup packet — in the same one. Those bytes are already
        // decrypted and sitting in the session by the time this task starts. Draining only in the
        // socket arm below would leave them there until the peer sent *more*, which it will not:
        // it is waiting for the answer to the packet it already sent. That is a deadlock, and it
        // is the one this loop is shaped to avoid.
        if !deliver(&mut session, &mut session_tx, &mut decrypted).await? {
            return Ok(());
        }
        flush_tls(&mut session, &mut socket_tx).await?;
        // Both arms are cancel-safe reads, which is what makes `select!` correct here: the arm
        // that loses has not consumed anything.
        tokio::select! {
            read = socket_rx.read(&mut from_socket) => {
                let read = read?;
                if read == 0 {
                    // The peer closed the connection. Dropping our side of the pipe is what tells
                    // the session above that its client is gone.
                    return Ok(());
                }
                feed_tls(&mut session, &from_socket[..read])?;
            }
            written = session_rx.read(&mut from_session) => {
                let written = written?;
                if written == 0 {
                    // The protocol loop finished. Tell the peer so rather than dropping the
                    // socket: an unannounced close is indistinguishable from a truncation attack,
                    // and a client that checks will say so.
                    session.send_close_notify();
                    flush_tls(&mut session, &mut socket_tx).await?;
                    return Ok(());
                }
                std::io::Write::write_all(&mut session.writer(), &from_session[..written])?;
            }
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
            // The quantum holds exactly 24 bits, so the low three bytes of its big-endian form
            // *are* the output: no cast, no mask, nothing to get wrong.
            let [_, first, second, third] = quantum.to_be_bytes();
            out.extend_from_slice(&[first, second, third]);
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
            let [_, first, second, _] = (quantum << 6).to_be_bytes();
            out.extend_from_slice(&[first, second]);
            Ok(out)
        }
        (2, 2) => {
            let [_, only, _, _] = (quantum << 12).to_be_bytes();
            out.push(only);
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
        assert_eq!(
            base64_decode("Zm9v\nYmFy\r\n").as_deref(),
            Ok(&b"foobar"[..])
        );
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
