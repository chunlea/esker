//! HTTPS to the object store, and the only module in this crate that knows what TLS is.
//!
//! The rule `pgwire::tls` lives under applies here too: a `use rustls::` anywhere else in this
//! crate is a review failure. Everything above sees a [`Transport`], which is what
//! [ADR 0025](../../../docs/adr/0025-s3-transport-and-tls.md) decision 1 built the seam for —
//! "when TLS lands it is a second implementor here, not a change anywhere else", and this is that
//! second implementor.
//!
//! # Why this is so much smaller than the PostgreSQL port's
//!
//! That surface is `tokio`, so driving a sans-io TLS state machine there meant a task and a pipe
//! ([ADR 0055](../../../docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)). This
//! crate is **blocking** by rule (`CLAUDE.md`: the engine stays synchronous), and blocking is the
//! case rustls has a ready answer for: `StreamOwned` is a `Read + Write` over a session and a
//! socket. So the interesting code here is not the plumbing — it is the root store and the
//! connection pool.
//!
//! # The root store, and why no crate was added for it
//!
//! ADR 0055 predicted this surface would want `webpki-roots`: one crate, plus a
//! `CDLA-Permissive-2.0` line in `deny.toml`'s licence allow-list. It does not. A CA bundle is a
//! PEM file, every platform this project builds for ships one, and reading a file is not a
//! dependency — so [`TlsRoots::Platform`] reads the first bundle it finds (honouring
//! `SSL_CERT_FILE`, which is the conventional override) and [`TlsRoots::File`] reads the one an
//! operator names. **The TLS exception stays at nine crates across both surfaces.**
//!
//! The trade is honest and worth stating: vendored roots are the same on every machine, and a
//! bundle read from disk is whatever the machine has. For a database talking to its own object
//! store, that is the behaviour an operator expects — the same trust the rest of the host has —
//! and `ESKER_S3_CA_CERT` is there for the self-signed case, which is the one that actually comes
//! up (a `MinIO` in a container).

use std::path::PathBuf;
/// The standard `Result`, under a name the crate's own `Result` alias cannot shadow.
///
/// The two PEM readers below answer with it: their errors are strings about a file's contents,
/// which the caller turns into an [`Error::Config`] naming the file. Renaming rather than
/// qualifying keeps them identical in both builds — with the feature off, the crate alias is not
/// in scope and `std::result::Result` would be a redundant path.
#[cfg(any(feature = "tls", test))]
use std::result::Result as StdResult;

#[cfg(feature = "tls")]
use std::fmt;
#[cfg(feature = "tls")]
use std::path::Path;

#[cfg(feature = "tls")]
use crate::error::{Error, Result};
#[cfg(feature = "tls")]
use std::io::Write;
#[cfg(feature = "tls")]
use std::net::TcpStream;
#[cfg(feature = "tls")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "tls")]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "tls")]
use crate::http::{Response, read_response};
#[cfg(feature = "tls")]
use crate::transport::{Timeouts, Transport, dial};

#[cfg(feature = "tls")]
/// How many idle TLS connections are kept per endpoint.
///
/// The same bound [`crate::transport::TcpTransport`] uses, for the same reason, and it matters
/// more here: a TLS connection costs a handshake as well as a round trip to open.
const MAX_IDLE: usize = 16;

/// Where the roots that verify an `https://` endpoint come from.
///
/// Present in both builds so that [`crate::Config`] needs no `cfg`; without the `tls` feature
/// nothing can reach a TLS endpoint to use it, because [`crate::Endpoint::parse`] refuses one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TlsRoots {
    /// The host's own CA bundle: `$SSL_CERT_FILE` if it is set, else the first of the usual
    /// platform paths that exists.
    #[default]
    Platform,
    /// One PEM file, named by an operator — `ESKER_S3_CA_CERT`. What a self-signed `MinIO` needs,
    /// and the only way to trust one without trusting it for the whole machine.
    File(PathBuf),
}

#[cfg(feature = "tls")]
/// Where a CA bundle usually lives, in the order they are tried.
///
/// Every entry is a real distribution's path. macOS has no bundle file at all by default — its
/// roots are in the keychain, which is not readable without linking Security.framework — so a
/// developer there points `ESKER_S3_CA_CERT` at one, which is what the self-signed case does
/// anyway.
const BUNDLE_PATHS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt", // Debian, Ubuntu, Alpine
    "/etc/pki/tls/certs/ca-bundle.crt",   // Fedora, RHEL
    "/etc/ssl/ca-bundle.pem",             // openSUSE
    "/etc/ssl/cert.pem",                  // Alpine, BSD, some macOS installs
];

#[cfg(feature = "tls")]
impl TlsRoots {
    /// The file this resolves to, or an error naming what was looked for.
    fn path(&self) -> Result<PathBuf> {
        match self {
            Self::File(path) => Ok(path.clone()),
            Self::Platform => {
                if let Some(named) = std::env::var_os("SSL_CERT_FILE") {
                    return Ok(PathBuf::from(named));
                }
                BUNDLE_PATHS
                    .iter()
                    .map(Path::new)
                    .find(|path| path.is_file())
                    .map(Path::to_path_buf)
                    .ok_or_else(|| {
                        Error::Config(format!(
                            "no CA bundle found in any of {}: set SSL_CERT_FILE or \
                             ESKER_S3_CA_CERT to a PEM file of trusted roots",
                            BUNDLE_PATHS.join(", ")
                        ))
                    })
            }
        }
    }
}

#[cfg(feature = "tls")]
/// HTTPS over a pool of kept-alive TLS connections, one pool per endpoint.
pub struct TlsTransport {
    /// The timeouts every connection gets.
    timeouts: Timeouts,
    /// Where the roots come from.
    roots: TlsRoots,
    /// The client configuration, built once on first use — or the reason it could not be.
    ///
    /// Built lazily so that [`crate::S3Client::new`] can stay infallible while a bad CA file is
    /// still *loud*: every request fails with the reason, and none of them falls back to
    /// plaintext. [`crate::S3Client::open`] builds it eagerly instead, so a server started with a
    /// bad CA file fails at startup rather than at its first upload.
    client: OnceLock<StdResult<Arc<rustls::ClientConfig>, String>>,
    /// Sessions at a message boundary, waiting for the next request.
    idle: Mutex<Vec<Idle>>,
    /// How many TLS connections this transport has opened, ever.
    opened: AtomicU64,
}

#[cfg(feature = "tls")]
/// One pooled session and the endpoint it belongs to.
struct Idle {
    host: String,
    port: u16,
    stream: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

#[cfg(feature = "tls")]
impl fmt::Debug for TlsTransport {
    /// Deliberately shallow: a `ClientConfig` prints its whole cipher-suite table, and an `Idle`
    /// holds a live session.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsTransport")
            .field("roots", &self.roots)
            .field("opened", &self.connections_opened())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "tls")]
impl TlsTransport {
    /// A transport that builds its client configuration on first use.
    #[must_use]
    pub fn new(roots: TlsRoots) -> Self {
        Self::with_timeouts(roots, Timeouts::default())
    }

    /// A transport with explicit timeouts.
    #[must_use]
    pub fn with_timeouts(roots: TlsRoots, timeouts: Timeouts) -> Self {
        Self {
            timeouts,
            roots,
            client: OnceLock::new(),
            idle: Mutex::new(Vec::new()),
            opened: AtomicU64::new(0),
        }
    }

    /// A transport whose configuration is built **now**, so a bad CA file is a startup error.
    ///
    /// # Errors
    ///
    /// The bundle could not be found or read, held no certificate this build can parse, or
    /// `rustls` refused the resulting configuration.
    pub fn prepared(roots: TlsRoots, timeouts: Timeouts) -> Result<Self> {
        let transport = Self::with_timeouts(roots, timeouts);
        transport.client_config()?;
        Ok(transport)
    }

    /// How many TLS connections this transport has opened since it was created.
    ///
    /// The observable the keep-alive is asserted against, exactly as on the plain transport: reuse
    /// is invisible from the responses, which are the same responses either way.
    #[must_use]
    pub fn connections_opened(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }

    /// Closes every idle session, keeping none.
    pub fn close_idle(&self) {
        self.pool().clear();
    }

    /// The client configuration, building it on the first call.
    fn client_config(&self) -> Result<Arc<rustls::ClientConfig>> {
        self.client
            .get_or_init(|| build_client_config(&self.roots).map_err(|error| error.to_string()))
            .clone()
            .map_err(Error::Config)
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, Vec<Idle>> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A pooled session to `host:port` that still looks usable, if there is one.
    fn take_idle(
        &self,
        host: &str,
        port: u16,
    ) -> Option<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        loop {
            let mut stream = {
                let mut pool = self.pool();
                let at = pool
                    .iter()
                    .rposition(|idle| idle.port == port && idle.host == host)?;
                pool.remove(at).stream
            };
            if is_alive(&mut stream) {
                return Some(stream);
            }
            tracing::trace!(host, port, "an idle S3 TLS session had ended");
        }
    }

    /// Puts a session back at a message boundary.
    fn put_idle(
        &self,
        host: &str,
        port: u16,
        stream: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    ) {
        let mut pool = self.pool();
        if pool.len() >= MAX_IDLE {
            return;
        }
        pool.push(Idle {
            host: host.to_string(),
            port,
            stream,
        });
    }

    /// Opens a TCP connection, wraps it in a session, and completes the handshake.
    ///
    /// The handshake is driven **here** rather than left to the first write, so that a rejected
    /// certificate is a [`Error::Tls`] naming the endpoint instead of an I/O error surfacing from
    /// the middle of a `PutObject`.
    fn connect(
        &self,
        host: &str,
        port: u16,
    ) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        let config = self.client_config()?;
        // The name is what the certificate is checked against, and it is the endpoint's host —
        // never the address it resolved to. It is also what goes in the SNI extension, so a
        // server hosting several buckets' names sends back the right chain.
        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| Error::Tls(format!("{host} is not a valid name for a certificate")))?;
        let session = rustls::ClientConnection::new(config, server_name)
            .map_err(|error| Error::Tls(format!("starting a TLS session with {host}: {error}")))?;
        let socket = dial(host, port, self.timeouts)?;
        let mut stream = rustls::StreamOwned::new(session, socket);
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|error| handshake_error(host, port, &error))?;
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(stream)
    }
}

#[cfg(feature = "tls")]
impl Transport for TlsTransport {
    fn round_trip(&self, host: &str, port: u16, request: &[u8]) -> Result<Response> {
        let mut stream = match self.take_idle(host, port) {
            Some(kept) => kept,
            None => self.connect(host, port)?,
        };

        // As on the plain transport: a failure anywhere below drops the session rather than
        // pooling it, because a session whose position in the message stream is unknown is the one
        // thing that must never come back out of the pool.
        stream
            .write_all(request)
            .map_err(|source| Error::io("sending the request", source))?;
        stream
            .flush()
            .map_err(|source| Error::io("sending the request", source))?;

        let response = read_response(&mut stream)?;
        if response.may_reuse_connection() {
            self.put_idle(host, port, stream);
        }
        Ok(response)
    }
}

#[cfg(feature = "tls")]
/// Turns a handshake failure into the right kind of error.
///
/// The split matters because `esker-engine`'s uploader retries forever on a retryable error
/// (ADR 0024 decision 2). A connection reset during a handshake is a server restarting and is
/// worth waiting for; a certificate that does not verify is a standing misconfiguration, and
/// retrying it forever would hide the one thing an operator needs to be told.
fn handshake_error(host: &str, port: u16, error: &std::io::Error) -> Error {
    use std::io::ErrorKind::{
        BrokenPipe, ConnectionAborted, ConnectionRefused, ConnectionReset, NotConnected, TimedOut,
        UnexpectedEof, WouldBlock,
    };
    if matches!(
        error.kind(),
        ConnectionReset
            | ConnectionAborted
            | ConnectionRefused
            | BrokenPipe
            | NotConnected
            | TimedOut
            | WouldBlock
            | UnexpectedEof
    ) {
        return Error::io(
            "the TLS handshake",
            std::io::Error::new(error.kind(), error.to_string()),
        );
    }
    Error::Tls(format!(
        "the TLS handshake with {host}:{port} failed: {error}"
    ))
}

#[cfg(feature = "tls")]
/// Whether an idle session is still at a message boundary and usable.
///
/// **This is not the plain transport's TCP peek, and it must not be.** A TLS 1.3 server sends
/// session tickets *after* the handshake, whenever it likes — so on a healthy, idle, perfectly
/// reusable connection there are usually bytes waiting on the socket. A peek that treats "bytes
/// are readable" as "not at a boundary" would condemn almost every pooled session, and the
/// keep-alive ADR 0039 measured would quietly stop working while every response stayed correct.
///
/// So the pending bytes are *absorbed* instead: fed to the session, which consumes tickets and key
/// updates itself. What disqualifies a connection is a session that produces **plaintext** — the
/// server talking out of turn, which means the boundary is not where the pool believes — or an
/// error, or an orderly close.
fn is_alive(stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>) -> bool {
    if stream.sock.set_nonblocking(true).is_err() {
        return false;
    }
    let alive = absorb_pending(stream);
    // Back to blocking whatever the answer was: a socket left non-blocking would turn every
    // subsequent read into a spurious `WouldBlock`.
    if stream.sock.set_nonblocking(false).is_err() {
        return false;
    }
    alive
}

#[cfg(feature = "tls")]
/// Feeds whatever is waiting on the socket to the session, and says whether it stayed healthy.
fn absorb_pending(stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>) -> bool {
    loop {
        match stream.conn.read_tls(&mut stream.sock) {
            // Nothing waiting: the session is idle and at a boundary, which is the common case.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
            // A broken socket, or an orderly close: either way this session is finished.
            Err(_) | Ok(0) => return false,
            Ok(_) => match stream.conn.process_new_packets() {
                Err(_) => return false,
                Ok(state) => {
                    if state.plaintext_bytes_to_read() > 0 {
                        return false;
                    }
                    if state.peer_has_closed() {
                        return false;
                    }
                }
            },
        }
    }
}

#[cfg(feature = "tls")]
/// Builds the `rustls` client configuration from a root source.
fn build_client_config(roots: &TlsRoots) -> Result<Arc<rustls::ClientConfig>> {
    let path = roots.path()?;
    let pem = std::fs::read_to_string(&path).map_err(|source| {
        Error::Config(format!(
            "reading the CA bundle {}: {source}",
            path.display()
        ))
    })?;
    let certificates = decode_pem(&pem, "CERTIFICATE")
        .map_err(|reason| Error::Config(format!("{}: {reason}", path.display())))?;
    if certificates.is_empty() {
        return Err(Error::Config(format!(
            "{} holds no CERTIFICATE block",
            path.display()
        )));
    }

    let mut store = rustls::RootCertStore::empty();
    // A real bundle holds a hundred or more certificates and may hold one this build cannot
    // parse; that is not a reason to trust nothing. What *is* a reason to stop is none of them
    // parsing, which means the file is not what it was taken for.
    let (added, ignored) = store.add_parsable_certificates(
        certificates
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from),
    );
    if added == 0 {
        return Err(Error::Config(format!(
            "{}: none of its {ignored} certificate(s) could be parsed",
            path.display()
        )));
    }
    if ignored > 0 {
        tracing::debug!(
            path = %path.display(),
            added,
            ignored,
            "some certificates in the CA bundle were not parsable"
        );
    }

    let provider = Arc::new(rustls_graviola::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Tls(format!("building the TLS configuration: {error}")))?
        .with_root_certificates(store)
        .with_no_client_auth();
    tracing::debug!(path = %path.display(), roots = added, "loaded the S3 trust roots");
    Ok(Arc::new(config))
}

#[cfg(any(feature = "tls", test))]
/// Pulls every `-----BEGIN <label>-----` … `-----END <label>-----` block out of `text`.
///
/// A second copy of what `esker_sql::pgwire::tls` does, because the two crates share no home for
/// it: `esker-base` is where it belongs and this unit does not own that file. It is written down
/// as owed rather than left for someone to find twice.
fn decode_pem(text: &str, label: &str) -> StdResult<Vec<Vec<u8>>, String> {
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
/// Decodes standard base64, ignoring the whitespace PEM wraps with.
///
/// Padding is required, as it is in `pgwire::tls`: PEM always pads, and a file that does not is
/// malformed rather than an invitation to guess.
fn base64_decode(text: &str) -> StdResult<Vec<u8>, String> {
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
            let [_, first, second, third] = quantum.to_be_bytes();
            out.extend_from_slice(&[first, second, third]);
            quantum = 0;
            filled = 0;
        }
    }
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

    #[test]
    fn decodes_the_rfc_4648_vectors() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(base64_decode(encoded).as_deref(), Ok(plain.as_bytes()));
        }
    }

    #[test]
    fn malformed_pem_is_an_error_not_a_panic() {
        assert!(base64_decode("Zm9vYmFy!").is_err());
        assert!(base64_decode("Z").is_err());
        assert!(decode_pem("-----BEGIN CERTIFICATE-----\nZm9v\n", "CERTIFICATE").is_err());
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_bundle_that_is_not_a_bundle_is_refused_by_name() {
        let directory = std::env::temp_dir().join(format!("esker-s3-tls-{}", std::process::id()));
        std::fs::create_dir_all(&directory).ok();
        let path = directory.join("not-a-bundle.pem");
        std::fs::write(&path, "there are no certificates here\n").ok();

        let Err(error) = build_client_config(&TlsRoots::File(path.clone())) else {
            panic!("a file with no CERTIFICATE block must be refused");
        };
        assert!(error.to_string().contains("not-a-bundle.pem"), "{error}");
        assert!(!error.is_retryable(), "a bad CA file is not worth retrying");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_missing_bundle_names_what_it_looked_for() {
        let Err(error) = build_client_config(&TlsRoots::File("/nonexistent/ca.pem".into())) else {
            panic!("a missing CA file must be refused");
        };
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
    }
}
