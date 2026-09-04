//! TLS for the framed RPC, and the one TLS session driver the project has.
//!
//! Three surfaces needed TLS ([ADR 0055](../../../../docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)):
//! the PostgreSQL port, the S3 tier and this one. The S3 client blocks, so `rustls::StreamOwned`
//! serves it directly. The other two are `tokio`, and driving a sans-io state machine on an async
//! socket is the part worth writing exactly once — so it is written here, in the crate that
//! already owns `tokio` and the framing, and `esker_sql::pgwire::tls` is a caller rather than a
//! second copy.
//!
//! # Why a task and a pipe rather than a poll adapter
//!
//! `rustls` is sans-io: bytes in, bytes out, and something has to carry them between the socket
//! and the session. The usual answer is a stream adapter implementing `poll_read`/`poll_write`
//! over the state machine — which is what `tokio-rustls` is, and it is a crate this project has
//! not taken (the exception ADR 0055 records names two crates, and this would be a third).
//!
//! Writing that adapter by hand means hand-written `Poll` code where a missed wakeup is a hung
//! connection that reproduces once a week. This does the same job with a task and a duplex pipe:
//! ordinary `async` code, cancel-safe `select!` arms, no manual `Poll` anywhere. It costs one task
//! and one copy per direction, which a protocol of small framed messages will not notice.
//!
//! **The known limit**: the pump reads from the socket only while the session above is keeping up,
//! so a peer that floods while refusing to read its own replies can stall a connection rather than
//! being backpressured into an error. Request/response traffic cannot reach that state. It is
//! written down because it is the first thing to look at if a connection ever hangs with data
//! pending.
//!
//! # What is *not* here
//!
//! Authorisation. A verified certificate says the peer holds a key some CA vouched for; it does
//! not say that peer may register as store 7 or vote in region 4. Nothing in this project maps an
//! identity to a right yet, and mTLS below makes the identity *available* to check rather than
//! checking it. ADR 0055 says so in the same words, so that turning this on is not mistaken for
//! having closed that gap.

use std::path::{Path, PathBuf};

#[cfg(feature = "tls")]
use std::sync::Arc;

/// What went wrong configuring RPC TLS.
///
/// Every one of these is a *startup* failure: a node refuses to come up rather than coming up
/// without the encryption it was told to provide.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Certificate flags were given to a binary built without the `tls` feature.
    #[error(
        "TLS was configured ({path}) but this binary was built without it: rebuild with \
         `--features tls`, or remove the RPC TLS flags and terminate TLS in front of this node \
         (docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)"
    )]
    NotCompiledIn {
        /// The file the operator named, so the message says which flag was believed.
        path: PathBuf,
    },
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why not.
        source: std::io::Error,
    },
    /// A file held no PEM block of the expected kind, or one that would not decode.
    #[error("{path}: {reason}")]
    Malformed {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
    /// `rustls` refused the material as given.
    #[error("rustls rejected the TLS configuration: {0}")]
    Rejected(String),
}

/// What a node was told about RPC TLS.
///
/// Present in both builds so that callers need no `cfg`; without the `tls` feature
/// [`RpcTls::from_files`] is an error rather than a quietly disabled configuration, which is the
/// discipline every surface in ADR 0055 follows.
///
/// One value carries both directions on purpose. A store is a server to its clients and a client
/// to its peers and to PD, and giving it two configurations built from the same three files would
/// be two chances to configure one node inconsistently.
#[derive(Clone, Default)]
pub struct RpcTls {
    #[cfg(feature = "tls")]
    inner: Option<Arc<Inner>>,
}

#[cfg(feature = "tls")]
struct Inner {
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    /// Whether this node demands a certificate from peers that connect to it.
    mutual: bool,
}

impl std::fmt::Debug for RpcTls {
    /// Deliberately opaque: these configurations hold key material, and `TransportConfig`-shaped
    /// things get printed in log lines.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcTls")
            .field("enabled", &self.is_enabled())
            .field("mutual", &self.is_mutual())
            .finish()
    }
}

impl RpcTls {
    /// A node that speaks no TLS. The default, and what every existing caller gets.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Whether this node has TLS configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            self.inner.is_some()
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }

    /// Whether peers must present a certificate of their own (mTLS).
    #[must_use]
    pub fn is_mutual(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            self.inner.as_ref().is_some_and(|inner| inner.mutual)
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }

    /// Builds a configuration from this node's certificate, its key, and the roots it trusts.
    ///
    /// `mutual` turns on client certificates in both directions: this node presents its own
    /// certificate when it connects to a peer, and demands one from a peer that connects to it.
    /// That is the setting for store↔store and PD↔PD, where both ends are ours; a SQL client
    /// reaching a store has no certificate and would be refused by a server that required one.
    ///
    /// # Errors
    ///
    /// [`TlsError::NotCompiledIn`] without the `tls` feature — never a silently disabled
    /// configuration. Otherwise a file that could not be read, held no usable PEM block, or that
    /// `rustls` refused.
    #[cfg_attr(
        not(feature = "tls"),
        expect(unused_variables, reason = "there is no TLS to configure")
    )]
    pub fn from_files(
        certificate: &Path,
        key: &Path,
        roots: &Path,
        mutual: bool,
    ) -> Result<Self, TlsError> {
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
            if chain.is_empty() {
                return Err(TlsError::Malformed {
                    path: certificate.to_path_buf(),
                    reason: "no CERTIFICATE block".to_owned(),
                });
            }

            // The three labels a PEM private key comes under, in the order a generated key is most
            // likely to carry. A label that is not one of them is an error, never an attempt to
            // read it as something else.
            let key_der = if let Some(der) = read_first(key, "PRIVATE KEY")? {
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der))
            } else if let Some(der) = read_first(key, "EC PRIVATE KEY")? {
                PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der))
            } else if let Some(der) = read_first(key, "RSA PRIVATE KEY")? {
                PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der))
            } else {
                return Err(TlsError::Malformed {
                    path: key.to_path_buf(),
                    reason: "no PRIVATE KEY block".to_owned(),
                });
            };

            let mut store = rustls::RootCertStore::empty();
            let (added, ignored) = store.add_parsable_certificates(
                read_pem(roots, "CERTIFICATE")?
                    .into_iter()
                    .map(CertificateDer::from),
            );
            if added == 0 {
                return Err(TlsError::Malformed {
                    path: roots.to_path_buf(),
                    reason: format!("none of its {ignored} certificate(s) could be parsed"),
                });
            }
            let store = Arc::new(store);

            // Passed rather than installed: `install_default` is process-global and would decide
            // for everything else linking rustls in this process.
            let provider = Arc::new(rustls_graviola::default_provider());

            let client = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(|error| TlsError::Rejected(error.to_string()))?
                .with_root_certificates(Arc::clone(&store));
            let client = if mutual {
                client
                    .with_client_auth_cert(chain.clone(), key_der.clone_key())
                    .map_err(|error| TlsError::Rejected(error.to_string()))?
            } else {
                client.with_no_client_auth()
            };

            let server = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|error| TlsError::Rejected(error.to_string()))?;
            let server = if mutual {
                let verifier =
                    rustls::server::WebPkiClientVerifier::builder_with_provider(store, {
                        // The verifier needs a provider of its own; it is the same one.
                        Arc::new(rustls_graviola::default_provider())
                    })
                    .build()
                    .map_err(|error| TlsError::Rejected(error.to_string()))?;
                server.with_client_cert_verifier(verifier)
            } else {
                server.with_no_client_auth()
            };
            let server = server
                .with_single_cert(chain, key_der)
                .map_err(|error| TlsError::Rejected(error.to_string()))?;

            Ok(Self {
                inner: Some(Arc::new(Inner {
                    client: Arc::new(client),
                    server: Arc::new(server),
                    mutual,
                })),
            })
        }
    }

    /// The client half, for a connection this node is making.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn client(&self) -> Option<Arc<rustls::ClientConfig>> {
        self.inner.as_ref().map(|inner| Arc::clone(&inner.client))
    }

    /// The server half, for a connection this node is accepting.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn server(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.inner.as_ref().map(|inner| Arc::clone(&inner.server))
    }
}

/// Every PEM block of `label` in `path`, read and decoded.
#[cfg(feature = "tls")]
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

/// The first PEM block of `label` in `path`, or `None`.
#[cfg(feature = "tls")]
fn read_first(path: &Path, label: &'static str) -> Result<Option<Vec<u8>>, TlsError> {
    Ok(read_pem(path, label)?.into_iter().next())
}

/// A connection, before or after it became a TLS one.
///
/// The framed reader and writer are generic over their stream, so this exists only to give the
/// two cases one type at the point where a socket is handed on.
pub enum MaybeTlsStream<S> {
    /// The socket as it arrived.
    Plain(S),
    /// The plaintext side of a TLS session; the ciphertext side belongs to the pump task.
    #[cfg(feature = "tls")]
    Tls(tokio::io::DuplexStream),
}

impl<S> std::fmt::Debug for MaybeTlsStream<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// How much plaintext may be in flight in each direction between a session and its pump.
#[cfg(feature = "tls")]
const PLAINTEXT_BUFFER: usize = 64 * 1024;

/// Completes a server handshake on `socket` and returns the plaintext side of the session.
///
/// # Errors
///
/// Anything the socket does, and any TLS failure: a malformed `ClientHello`, no overlap in
/// versions or suites, a client that goes away mid-handshake, or — under mTLS — a client whose
/// certificate does not verify. All of them are error values that end one connection.
#[cfg(feature = "tls")]
pub async fn accept<S>(
    socket: S,
    config: Arc<rustls::ServerConfig>,
) -> std::io::Result<tokio::io::DuplexStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let session = rustls::ServerConnection::new(config).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    drive(socket, rustls::Connection::Server(session)).await
}

/// Completes a client handshake on `socket` and returns the plaintext side of the session.
///
/// `name` is what the peer's certificate is checked against — the configured address's host, never
/// the address it resolved to — and is also what goes out in SNI.
///
/// # Errors
///
/// As [`accept`], plus a certificate that does not chain to a trusted root or does not cover
/// `name`.
#[cfg(feature = "tls")]
pub async fn connect<S>(
    socket: S,
    config: Arc<rustls::ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
) -> std::io::Result<tokio::io::DuplexStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let session = rustls::ClientConnection::new(config, name).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    drive(socket, rustls::Connection::Client(session)).await
}

/// Runs the handshake to completion and hands the data phase to a task.
#[cfg(feature = "tls")]
async fn drive<S>(
    mut socket: S,
    mut session: rustls::Connection,
) -> std::io::Result<tokio::io::DuplexStream>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    let mut buffer = vec![0u8; 8 * 1024];
    while session.is_handshaking() {
        flush(&mut session, &mut socket).await?;
        if !session.is_handshaking() {
            break;
        }
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the peer went away during the TLS handshake",
            ));
        }
        feed(&mut session, &buffer[..read])?;
    }
    // The last flight of the handshake is still queued at this point.
    flush(&mut session, &mut socket).await?;

    let (session_side, caller_side) = tokio::io::duplex(PLAINTEXT_BUFFER);
    tokio::spawn(async move {
        if let Err(error) = pump(socket, session, session_side).await {
            tracing::debug!(%error, "a TLS session ended");
        }
    });
    Ok(caller_side)
}

/// Writes whatever the session has queued.
#[cfg(feature = "tls")]
async fn flush<S>(session: &mut rustls::Connection, socket: &mut S) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    while session.wants_write() {
        let mut encrypted = Vec::new();
        // Writing into a `Vec` cannot fail, so a zero here only means nothing was queued.
        if session.write_tls(&mut encrypted)? == 0 {
            break;
        }
        socket.write_all(&encrypted).await?;
    }
    socket.flush().await
}

/// Feeds ciphertext to the session and advances its state machine.
#[cfg(feature = "tls")]
fn feed(session: &mut rustls::Connection, mut ciphertext: &[u8]) -> std::io::Result<()> {
    while !ciphertext.is_empty() {
        // `read_tls` takes what fits in the session's buffer, which may be less than is offered.
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

/// Moves everything already decrypted up to the caller. `false` when the peer closed cleanly.
#[cfg(feature = "tls")]
async fn deliver<W>(
    session: &mut rustls::Connection,
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
    mut session: rustls::Connection,
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
        // **Deliver first, and before waiting on the socket.** A peer may put its `Finished` and
        // its first application data in one TCP segment, so those bytes are already decrypted
        // before this task starts. Draining only on socket activity would leave them there while
        // the peer waits for an answer to what it has already sent — a deadlock, and the reason
        // this loop is shaped this way.
        if !deliver(&mut session, &mut session_tx, &mut decrypted).await? {
            return Ok(());
        }
        flush(&mut session, &mut socket_tx).await?;
        // Both arms are cancel-safe reads, which is what makes `select!` correct here: the arm
        // that loses has not consumed anything.
        tokio::select! {
            read = socket_rx.read(&mut from_socket) => {
                let read = read?;
                if read == 0 {
                    return Ok(());
                }
                feed(&mut session, &from_socket[..read])?;
            }
            written = session_rx.read(&mut from_session) => {
                let written = written?;
                if written == 0 {
                    // The framed layer finished. Say so rather than dropping the socket: an
                    // unannounced close is indistinguishable from a truncation attack.
                    session.send_close_notify();
                    flush(&mut session, &mut socket_tx).await?;
                    return Ok(());
                }
                std::io::Write::write_all(&mut session.writer(), &from_session[..written])?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RpcTls;
    #[cfg(not(feature = "tls"))]
    use super::TlsError;
    #[cfg(not(feature = "tls"))]
    use std::path::Path;

    #[test]
    fn a_disabled_config_says_so_and_keeps_key_material_out_of_debug() {
        let tls = RpcTls::disabled();
        assert!(!tls.is_enabled());
        assert!(!tls.is_mutual());
        assert_eq!(
            format!("{tls:?}"),
            "RpcTls { enabled: false, mutual: false }"
        );
    }

    /// The refusal, in the build that has no TLS: configuring it is an error naming the way out,
    /// never a node that comes up speaking plaintext on a port believed to be encrypted.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn configuring_tls_without_the_feature_is_an_error() {
        let Err(error) = RpcTls::from_files(
            Path::new("/c.pem"),
            Path::new("/k.pem"),
            Path::new("/ca.pem"),
            true,
        ) else {
            panic!("a build without the feature must refuse to configure RPC TLS");
        };
        assert!(matches!(error, TlsError::NotCompiledIn { .. }));
        assert!(error.to_string().contains("--features tls"));
    }

    /// A file that is not a certificate is refused by name, and before anything is served.
    #[cfg(feature = "tls")]
    #[test]
    fn a_certificate_that_is_not_one_is_refused_by_name() {
        let directory =
            std::env::temp_dir().join(format!("esker-proto-tls-{}", std::process::id()));
        std::fs::create_dir_all(&directory).ok();
        let bad = directory.join("not-a-cert.pem");
        std::fs::write(&bad, "nothing here\n").ok();

        let Err(error) = RpcTls::from_files(&bad, &bad, &bad, false) else {
            panic!("a file with no CERTIFICATE block must be refused");
        };
        assert!(error.to_string().contains("not-a-cert.pem"), "{error}");
        std::fs::remove_dir_all(&directory).ok();
    }
}
