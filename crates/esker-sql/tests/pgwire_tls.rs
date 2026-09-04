//! TLS on the client port, in both builds of this crate.
//!
//! The tests here come in two kinds, and the split is the point:
//!
//! * **Both builds.** A node with no TLS configured answers `SSLRequest` with `N` and carries on in
//!   the clear. That is what almost every deployment of this node does today, so it is tested in
//!   the default build *and* in the one with the feature on — a refusal that only works when the
//!   code that could have said `S` is absent is not a tested refusal.
//! * **`--features tls` only.** A real `rustls` client, against a real listener, with the
//!   certificate checked in beside this file: handshake, startup packet, `SELECT 1`, all inside TLS
//!   records. `just check` runs `--all-features`, so these run in CI rather than being the kind of
//!   gated test that quietly never executes.
//!
//! The negative cases are here for invariant 9 (`CLAUDE.md`): every one of them is a client sending
//! bytes a server must survive. A malformed `ClientHello`, a truncated handshake, a client that
//! asks for TLS twice — each ends one connection with an error value and leaves the listener
//! serving.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::pgwire::server::{Auth, Config, Executors, NotYetExecuting, bind, serve_on};
use esker_sql::pgwire::session::Execute;
use esker_sql::pgwire::tls::TlsConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The `SSLRequest` code: 1234 in the high 16 bits, 5679 in the low.
const SSL_REQUEST: u32 = 80_877_103;
/// The `GSSENCRequest` code, the same trick one along.
const GSSENC_REQUEST: u32 = 80_877_104;

/// Hands every session the placeholder executor, as the other protocol tests do.
struct Sessions;

impl Executors for Sessions {
    fn for_session(&self, _database: &str) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Ok(Box::new(NotYetExecuting))
    }
}

/// The checked-in certificate and key. See `tests/fixtures/README.md`.
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Starts a listener on a port the operating system chooses, so two copies of this test cannot
/// collide, and returns the address.
async fn listen(tls: TlsConfig) -> std::net::SocketAddr {
    let listener = bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_on(
            listener,
            Config {
                address: address.to_string(),
                auth: Auth::Trust,
                tls,
                ..Config::default()
            },
            Arc::new(Sessions),
        )
        .await;
    });
    address
}

/// An eight-byte request packet: a length that counts itself, then the code.
fn request(code: u32) -> Vec<u8> {
    let mut packet = 8u32.to_be_bytes().to_vec();
    packet.extend_from_slice(&code.to_be_bytes());
    packet
}

/// A startup packet for protocol 3.0 naming a user and database.
fn startup_packet() -> Vec<u8> {
    let mut body = 0x0003_0000u32.to_be_bytes().to_vec();
    for (name, value) in [("user", "esker"), ("database", "esker")] {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = u32::try_from(body.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    packet.extend_from_slice(&body);
    packet
}

/// Reads framed messages until `ReadyForQuery`, and answers the tags seen.
///
/// **Bounded.** A protocol test that hangs tells you nothing and blocks the suite behind it; one
/// that gives up after ten seconds and prints the tags it did see names the message the server
/// stopped after. Ten seconds is far longer than any of these exchanges and far shorter than a CI
/// timeout.
async fn tags_until_ready<S: AsyncReadExt + Unpin>(stream: &mut S) -> String {
    let mut tags = String::new();
    loop {
        let mut header = [0u8; 5];
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_exact(&mut header),
        )
        .await;
        let Ok(read) = read else {
            panic!("the server stopped answering after {tags:?}");
        };
        if read.is_err() {
            return tags;
        }
        tags.push(header[0] as char);
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; length - 4];
        stream.read_exact(&mut body).await.unwrap();
        if header[0] == b'Z' {
            return tags;
        }
    }
}

/// The refusal, in whichever build is running: a node with no TLS configured says `N` and the
/// connection carries on in the clear.
///
/// **This runs with the feature on too.** With it on but no certificate given, the answer must
/// still be `N` — `--features tls` is a build-time capability, and `--tls-cert` is what turns it
/// on. A node that answered `S` because it *could* have would be offering a handshake it has no
/// key for.
#[tokio::test]
async fn a_node_with_no_certificate_refuses_and_continues_in_the_clear() {
    let address = listen(TlsConfig::disabled()).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();

    client.write_all(&request(SSL_REQUEST)).await.unwrap();
    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"N", "a bare N, with no length and no tag");

    // The same connection then completes an ordinary startup, which is what `sslmode=prefer` —
    // psql's and Rails' default — does after being refused.
    client.write_all(&startup_packet()).await.unwrap();
    assert!(tags_until_ready(&mut client).await.ends_with('Z'));
}

/// GSSAPI encryption is refused the same way, and the client may then ask about TLS.
///
/// `psql` with `gssencmode=prefer` sends this one *first*, so a server that mishandled it would
/// fail before the TLS question was ever asked.
#[tokio::test]
async fn a_gssapi_request_is_refused_and_the_client_may_still_ask_for_tls() {
    let address = listen(TlsConfig::disabled()).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();

    for code in [GSSENC_REQUEST, SSL_REQUEST] {
        client.write_all(&request(code)).await.unwrap();
        let mut answer = [0u8; 1];
        client.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"N");
    }
    client.write_all(&startup_packet()).await.unwrap();
    assert!(tags_until_ready(&mut client).await.ends_with('Z'));
}

/// A client that never asks about encryption is unaffected by any of this.
///
/// The accept path reads that first packet itself to answer `SSLRequest`; this is the test that it
/// hands an ordinary startup packet onwards intact rather than consuming it.
#[tokio::test]
async fn a_client_that_never_mentions_encryption_is_served_normally() {
    let address = listen(TlsConfig::disabled()).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(&startup_packet()).await.unwrap();
    assert!(tags_until_ready(&mut client).await.ends_with('Z'));
}

/// Configuring TLS in a build without the feature is a startup error, not a silent downgrade.
#[cfg(not(feature = "tls"))]
#[test]
fn a_build_without_the_feature_refuses_to_configure_tls() {
    let Err(error) = TlsConfig::from_pem_files(
        &fixture("localhost-test-cert.pem"),
        &fixture("localhost-test-key.pem"),
    ) else {
        panic!("a build without the `tls` feature must refuse a certificate it cannot serve");
    };
    let message = error.to_string();
    assert!(message.contains("--features tls"), "{message}");
}

#[cfg(feature = "tls")]
mod encrypted {
    use super::{SSL_REQUEST, fixture, listen, request, startup_packet, tags_until_ready};

    use std::sync::Arc;

    use esker_sql::pgwire::tls::TlsConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The certificate and key this crate ships for tests.
    fn server_tls() -> TlsConfig {
        TlsConfig::from_pem_files(
            &fixture("localhost-test-cert.pem"),
            &fixture("localhost-test-key.pem"),
        )
        .expect("the checked-in test certificate must load")
    }

    /// A `rustls` client that trusts the checked-in test CA and nothing else.
    ///
    /// No public root store: the trust anchor is the CA beside this file, which signed the
    /// server's leaf. Trusting exactly one CA is both what the test needs and what proves
    /// verification is really happening — a client that trusted anything would pass these tests
    /// against a certificate the server never sent.
    ///
    /// The leaf is a *leaf*: `CA:FALSE`, `serverAuth`, with the name in a SAN. A self-signed
    /// certificate with `CA:TRUE` — which is what `openssl req -x509` writes by default, and what
    /// this fixture was until the handshake refused it — is rejected by `rustls` as
    /// `CaUsedAsEndEntity`, correctly.
    fn client_config() -> Arc<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        let certificate = std::fs::read_to_string(fixture("ca-cert.pem")).unwrap();
        let der = pem_certificate(&certificate);
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .unwrap();
        let provider = Arc::new(rustls_graviola::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    /// The DER of the first certificate in a PEM file. The server has its own reader; this is the
    /// test's, kept separate on purpose so a bug in one cannot hide itself in the other.
    fn pem_certificate(text: &str) -> Vec<u8> {
        const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
        const END: &str = "-----END CERTIFICATE-----";
        let start = text.find(BEGIN).unwrap() + BEGIN.len();
        let end = text.find(END).unwrap();
        let payload: String = text[start..end].split_whitespace().collect();
        base64_decode(&payload)
    }

    /// Test-side base64, written the obvious way rather than the fast way.
    fn base64_decode(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut quantum = 0u32;
        let mut bits = 0;
        for byte in text.bytes().filter(|byte| *byte != b'=') {
            let value = u32::try_from(ALPHABET.iter().position(|c| *c == byte).unwrap()).unwrap();
            quantum = (quantum << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((quantum >> bits) & 0xff).unwrap());
            }
        }
        out
    }

    /// Answers `S`, completes a real handshake, and serves the whole session inside it.
    ///
    /// This is the test the whole unit exists for: a `rustls` client, a `rustls` server with the
    /// graviola provider, and the PostgreSQL startup sequence carried end to end over TLS.
    #[tokio::test]
    async fn a_client_that_asks_for_tls_gets_a_session_inside_it() {
        let address = listen(server_tls()).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();

        socket.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut answer = [0u8; 1];
        socket.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"S", "the server offers TLS");

        let mut stream = handshake(socket).await;
        // Everything from here is inside TLS records, the startup packet included.
        stream.write_all(&startup_packet()).await.unwrap();
        let tags = tags_until_ready(&mut stream).await;
        assert!(tags.starts_with('R'), "authentication first: {tags}");
        assert!(tags.ends_with('Z'), "ready for query: {tags}");

        // And a statement travels over it, which is what a client actually came for.
        let mut query = vec![b'Q'];
        let sql = b"SELECT 1\0";
        query.extend_from_slice(&u32::try_from(sql.len() + 4).unwrap().to_be_bytes());
        query.extend_from_slice(sql);
        stream.write_all(&query).await.unwrap();
        let answer = tags_until_ready(&mut stream).await;
        assert!(
            answer.ends_with('Z'),
            "the session answers over TLS and is ready again: {answer}"
        );
    }

    /// A second `SSLRequest`, sent *inside* the established session, is refused rather than
    /// starting a second handshake inside the first.
    #[tokio::test]
    async fn asking_for_tls_again_inside_tls_is_refused() {
        let address = listen(server_tls()).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut answer = [0u8; 1];
        socket.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"S");

        let mut stream = handshake(socket).await;
        stream.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut inner = [0u8; 1];
        stream.read_exact(&mut inner).await.unwrap();
        assert_eq!(&inner, b"N", "no nesting a session inside itself");

        // And the connection is still usable afterwards.
        stream.write_all(&startup_packet()).await.unwrap();
        assert!(tags_until_ready(&mut stream).await.ends_with('Z'));
    }

    /// Invariant 9, on the bytes an attacker chooses: garbage where a `ClientHello` belongs ends
    /// this connection and no other.
    #[tokio::test]
    async fn a_malformed_client_hello_ends_one_connection_and_not_the_listener() {
        let address = listen(server_tls()).await;
        let mut hostile = tokio::net::TcpStream::connect(address).await.unwrap();
        hostile.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut answer = [0u8; 1];
        hostile.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"S");
        // A TLS record header followed by nonsense, then silence.
        hostile
            .write_all(&[0x16, 0x03, 0x01, 0x00, 0x10, 0xff, 0xff, 0xff, 0xff])
            .await
            .unwrap();
        hostile.shutdown().await.ok();
        drop(hostile);

        // The listener is unharmed: a second client completes a whole session.
        let mut good = tokio::net::TcpStream::connect(address).await.unwrap();
        good.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut answer = [0u8; 1];
        good.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"S");
        let mut stream = handshake(good).await;
        stream.write_all(&startup_packet()).await.unwrap();
        assert!(tags_until_ready(&mut stream).await.ends_with('Z'));
    }

    /// A client that hangs up in the middle of the handshake is ordinary, not a crash.
    #[tokio::test]
    async fn a_truncated_handshake_is_an_error_not_a_panic() {
        let address = listen(server_tls()).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(&request(SSL_REQUEST)).await.unwrap();
        let mut answer = [0u8; 1];
        client.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"S");
        drop(client);

        // The listener still serves, which is the assertion: nothing above died with the peer.
        let mut good = tokio::net::TcpStream::connect(address).await.unwrap();
        good.write_all(&startup_packet()).await.unwrap();
        assert!(tags_until_ready(&mut good).await.ends_with('Z'));
    }

    /// A node with a certificate still serves a client that does not want TLS.
    ///
    /// `sslmode=disable` is a real setting and PostgreSQL's own `ssl = on` accepts both kinds of
    /// client; a node that refused plaintext once it had a certificate would break every existing
    /// caller the day an operator turned TLS on.
    #[tokio::test]
    async fn a_tls_node_still_serves_a_plaintext_client() {
        let address = listen(server_tls()).await;
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(&startup_packet()).await.unwrap();
        assert!(tags_until_ready(&mut client).await.ends_with('Z'));
    }

    /// A malformed PEM is an error value naming the file, never a panic.
    #[test]
    fn a_malformed_certificate_is_refused_by_name() {
        let directory = std::env::temp_dir().join(format!("esker-tls-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let bad = directory.join("not-a-cert.pem");
        std::fs::write(
            &bad,
            "-----BEGIN CERTIFICATE-----\nnot base64!!\n-----END CERTIFICATE-----\n",
        )
        .unwrap();

        let Err(error) = TlsConfig::from_pem_files(&bad, &fixture("localhost-test-key.pem")) else {
            panic!("a certificate that is not base64 must be refused");
        };
        assert!(error.to_string().contains("not-a-cert.pem"), "{error}");

        // And a file with no PEM block at all.
        let empty = directory.join("empty.pem");
        std::fs::write(&empty, "no certificate here\n").unwrap();
        let Err(error) = TlsConfig::from_pem_files(&empty, &fixture("localhost-test-key.pem"))
        else {
            panic!("a file with no CERTIFICATE block must be refused");
        };
        assert!(error.to_string().contains("CERTIFICATE"), "{error}");
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Drives the client half of the handshake and hands back the plaintext stream.
    async fn handshake(socket: tokio::net::TcpStream) -> tokio::io::DuplexStream {
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut session = rustls::ClientConnection::new(client_config(), server_name).unwrap();
        let (mut socket_rx, mut socket_tx) = tokio::io::split(socket);
        let mut buffer = vec![0u8; 8 * 1024];

        // The same shape the server side uses, and for the same reason: ordinary async code
        // rather than a hand-written `Poll` adapter.
        while session.is_handshaking() {
            while session.wants_write() {
                let mut encrypted = Vec::new();
                if session.write_tls(&mut encrypted).unwrap() == 0 {
                    break;
                }
                socket_tx.write_all(&encrypted).await.unwrap();
            }
            socket_tx.flush().await.unwrap();
            if !session.is_handshaking() {
                break;
            }
            let read = socket_rx.read(&mut buffer).await.unwrap();
            assert!(read > 0, "the server closed during the handshake");
            let mut cursor = &buffer[..read];
            while !cursor.is_empty() {
                session.read_tls(&mut cursor).unwrap();
                session.process_new_packets().unwrap();
            }
        }
        while session.wants_write() {
            let mut encrypted = Vec::new();
            if session.write_tls(&mut encrypted).unwrap() == 0 {
                break;
            }
            socket_tx.write_all(&encrypted).await.unwrap();
        }
        socket_tx.flush().await.unwrap();

        // From here the test talks plaintext to a task that encrypts for it, which keeps the tests
        // above readable as protocol tests rather than as TLS plumbing.
        let (mine, theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (mut plain_rx, mut plain_tx) = tokio::io::split(mine);
            let mut from_socket = vec![0u8; 8 * 1024];
            let mut from_plain = vec![0u8; 8 * 1024];
            let mut decrypted = vec![0u8; 8 * 1024];
            loop {
                while session.wants_write() {
                    let mut encrypted = Vec::new();
                    if session.write_tls(&mut encrypted).unwrap_or(0) == 0 {
                        break;
                    }
                    if socket_tx.write_all(&encrypted).await.is_err() {
                        return;
                    }
                }
                let _ = socket_tx.flush().await;
                tokio::select! {
                    read = socket_rx.read(&mut from_socket) => {
                        let Ok(read) = read else { return };
                        if read == 0 { return }
                        let mut cursor = &from_socket[..read];
                        while !cursor.is_empty() {
                            if session.read_tls(&mut cursor).is_err() { return }
                            if session.process_new_packets().is_err() { return }
                        }
                        loop {
                            match std::io::Read::read(&mut session.reader(), &mut decrypted) {
                                Ok(0) => return,
                                Ok(plain) => {
                                    if plain_tx.write_all(&decrypted[..plain]).await.is_err() {
                                        return;
                                    }
                                }
                                Err(error)
                                    if error.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(_) => return,
                            }
                        }
                    }
                    written = plain_rx.read(&mut from_plain) => {
                        let Ok(written) = written else { return };
                        if written == 0 { return }
                        if std::io::Write::write_all(
                            &mut session.writer(),
                            &from_plain[..written],
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });
        theirs
    }
}
