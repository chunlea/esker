//! The S3 client against a real TLS server, in this process.
//!
//! There is no TLS `MinIO` to point at — the container the acceptance tests use serves plain HTTP
//! on :9000 — so the server is here: a `rustls` listener that speaks just enough S3 to answer the
//! four calls and records what it was asked. That is a few hundred lines over the HTTP framing
//! this crate already owns, and writing it is cheaper than a dependency, which is the same trade
//! the client itself is (ADR 0025).
//!
//! What these tests are actually for:
//!
//! * **the happy path**, end to end, so the request that arrives over TLS is the same signed
//!   request the plain transport sends — same method, same path, same `Host`, same signature
//!   header;
//! * **the keep-alive**, because the TLS pool needed a different liveness check than the plain one
//!   and a wrong one would be invisible: every response would still be correct and every request
//!   would open a new connection. The assertion is on connections *accepted*, which is the only
//!   place that shows — and the test that matters is the one with a server that talks between
//!   requests, because a quiet server cannot tell the two checks apart;
//! * **the refusals**, one per way a certificate can be wrong, each checked for *which* error it
//!   is — a misconfiguration must not come back retryable, or the uploader will sit on it forever
//!   (ADR 0024 decision 2).
//!
//! The certificates beside this file are throwaway: see `tests/fixtures/README.md`.

#![cfg(feature = "tls")]
// A test fixture, written the plain way on purpose: `format!` into a `String` where a `write!`
// would need a trait import, and casts whose inputs are the few bytes this file itself wrote.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::format_push_string,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use esker_s3::transport::Transport;
use esker_s3::{Config, Credentials, Endpoint, ObjectStore, S3Client, TlsRoots};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// What one request looked like by the time it arrived.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    host: String,
    authorization: String,
    body: Vec<u8>,
    range: Option<(u64, u64)>,
}

/// A tiny S3 over TLS: enough to answer the four calls, and a record of what it was asked.
struct MockS3 {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Kept so the store outlives the connection threads that share it.
    _objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    /// How many TLS connections the server accepted, which is the other end of the client's own
    /// count and the thing the keep-alive assertion is really about.
    accepted: Arc<std::sync::atomic::AtomicU64>,
    /// Kept so the flag outlives the accept loop that reads it.
    _chatty: Arc<AtomicBool>,
}

impl MockS3 {
    /// Starts a server presenting `cert`/`key` on loopback.
    fn start(cert: &str, key: &str) -> Self {
        Self::with_chatter(cert, key, false)
    }

    /// A server that sends a post-handshake message after every response.
    fn chatty(cert: &str, key: &str) -> Self {
        Self::with_chatter(cert, key, true)
    }

    fn with_chatter(cert: &str, key: &str, talkative: bool) -> Self {
        let certs = load_certs(&fixture(cert));
        let key = load_key(&fixture(key));
        let provider = Arc::new(rustls_graviola::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let config = Arc::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let objects = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let chatty = Arc::new(AtomicBool::new(talkative));
        let chatty_thread = Arc::clone(&chatty);

        let server = MockS3 {
            address,
            seen: Arc::clone(&seen),
            _objects: Arc::clone(&objects),
            stop: Arc::clone(&stop),
            accepted: Arc::clone(&accepted),
            _chatty: chatty,
        };
        let chatty = chatty_thread;

        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(socket) = incoming else { return };
                accepted.fetch_add(1, Ordering::Relaxed);
                let config = Arc::clone(&config);
                let seen = Arc::clone(&seen);
                let objects = Arc::clone(&objects);
                let chatty = chatty.load(Ordering::Relaxed);
                // A thread per connection: this is a test fixture, and a connection here lives
                // for as long as the client keeps it alive, which is the point of the pool test.
                std::thread::spawn(move || {
                    let Ok(session) = rustls::ServerConnection::new(config) else {
                        return;
                    };
                    let mut stream = rustls::StreamOwned::new(session, socket);
                    // One connection, many requests — until the client stops talking.
                    while serve_one(&mut stream, &seen, &objects, chatty).unwrap_or(false) {}
                });
            }
        });
        server
    }

    fn url(&self) -> String {
        format!("https://localhost:{}", self.address.port())
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn connections_accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }
}

impl Drop for MockS3 {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so the thread notices the flag.
        let _ = TcpStream::connect(self.address);
    }
}

/// Reads one request and answers it. `Ok(true)` when the connection may carry another.
fn serve_one(
    stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
    seen: &Arc<Mutex<Vec<Seen>>>,
    objects: &Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    chatty: bool,
) -> std::io::Result<bool> {
    let Some(request) = read_request(stream)? else {
        return Ok(false);
    };
    seen.lock().unwrap().push(request.clone());

    let (status, body, extra) = answer(&request, objects);
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    if chatty {
        // A post-handshake message, sent *after* the response and therefore landing on a client
        // that is idle with the connection in its pool. A real TLS 1.3 server does this on its
        // own schedule — session tickets, key updates — and a client that mistakes it for "bytes
        // nobody asked for" throws away a perfectly good connection.
        let _ = stream.conn.refresh_traffic_keys();
        stream.flush()?;
    }
    Ok(true)
}

/// The S3 subset: PUT, GET (whole or ranged), DELETE, and `ListObjectsV2`.
fn answer(
    request: &Seen,
    objects: &Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
) -> (&'static str, Vec<u8>, Vec<(String, String)>) {
    let mut objects = objects.lock().unwrap();
    let (path, query) = match request.target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (request.target.as_str(), ""),
    };
    // Path-style addressing: /bucket/key.
    let key = path.trim_start_matches('/');
    let key = key.split_once('/').map_or("", |(_bucket, key)| key);

    match request.method.as_str() {
        "PUT" => {
            objects.insert(key.to_string(), request.body.clone());
            (
                "200 OK",
                Vec::new(),
                vec![("ETag".into(), "\"mock-etag\"".into())],
            )
        }
        "DELETE" => {
            objects.remove(key);
            ("204 No Content", Vec::new(), Vec::new())
        }
        "GET" if query.contains("list-type=2") => {
            let prefix = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("prefix="))
                .unwrap_or("")
                .replace("%2F", "/");
            let mut xml = String::from(
                "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
            );
            for (key, body) in objects.iter().filter(|(key, _)| key.starts_with(&prefix)) {
                xml.push_str(&format!(
                    "<Contents><Key>{key}</Key><Size>{}</Size><ETag>&quot;mock-etag&quot;</ETag></Contents>",
                    body.len()
                ));
            }
            xml.push_str("</ListBucketResult>");
            ("200 OK", xml.into_bytes(), Vec::new())
        }
        "GET" => {
            let Some(object) = objects.get(key) else {
                return (
                    "404 Not Found",
                    b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
                    Vec::new(),
                );
            };
            // A ranged GET answers 206 with a Content-Range, which is what the client checks.
            if let Some((from, to)) = request.range {
                let to = to.min(object.len().saturating_sub(1) as u64);
                let slice = object
                    .get(from as usize..=to as usize)
                    .unwrap_or_default()
                    .to_vec();
                return (
                    "206 Partial Content",
                    slice,
                    vec![
                        (
                            "Content-Range".into(),
                            format!("bytes {from}-{to}/{}", object.len()),
                        ),
                        ("ETag".into(), "\"mock-etag\"".into()),
                    ],
                );
            }
            (
                "200 OK",
                object.clone(),
                vec![("ETag".into(), "\"mock-etag\"".into())],
            )
        }
        _ => ("405 Method Not Allowed", Vec::new(), Vec::new()),
    }
}

/// Reads one HTTP/1.1 request: the line, the headers, and a `Content-Length` body.
fn read_request(
    stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
) -> std::io::Result<Option<Seen>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1..) => head.push(byte[0]),
            // End of stream, or a broken one: either way there is no request here.
            Ok(0) | Err(_) => return Ok(None),
        }
        if head.len() > 64 * 1024 {
            return Ok(None);
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut host = String::new();
    let mut authorization = String::new();
    let mut length = 0usize;
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "host" => host = value.to_string(),
            "authorization" => authorization = value.to_string(),
            "content-length" => length = value.parse().unwrap_or(0),
            "range" => {
                range = value.strip_prefix("bytes=").and_then(|spec| {
                    let (from, to) = spec.split_once('-')?;
                    Some((from.parse().ok()?, to.parse().ok()?))
                });
            }
            _ => {}
        }
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body)?;
    }
    Ok(Some(Seen {
        method,
        target,
        host,
        authorization,
        body,
        range,
    }))
}

fn load_certs(path: &std::path::Path) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    pem_blocks(&text, "CERTIFICATE")
        .into_iter()
        .map(rustls::pki_types::CertificateDer::from)
        .collect()
}

fn load_key(path: &std::path::Path) -> rustls::pki_types::PrivateKeyDer<'static> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let der = pem_blocks(&text, "PRIVATE KEY")
        .into_iter()
        .next()
        .expect("the fixture key is PKCS#8");
    rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(der))
}

/// The test's own PEM reader, kept separate from the client's on purpose: a bug in one must not
/// be able to hide itself in the other.
fn pem_blocks(text: &str, label: &str) -> Vec<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else { break };
        let payload: String = after[..stop].split_whitespace().collect();
        let mut out = Vec::new();
        let mut quantum = 0u32;
        let mut bits = 0;
        for byte in payload.bytes().filter(|byte| *byte != b'=') {
            let value = ALPHABET.iter().position(|c| *c == byte).unwrap() as u32;
            quantum = (quantum << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((quantum >> bits) & 0xff) as u8);
            }
        }
        blocks.push(out);
        rest = &after[stop + end.len()..];
    }
    blocks
}

/// A client for `server`, trusting `ca`.
fn client_for(server: &MockS3, ca: &str) -> S3Client {
    let endpoint = Endpoint::parse(&server.url()).unwrap();
    let mut config = Config::from_store_url(
        "s3://bucket/prefix",
        endpoint,
        "us-east-1",
        Credentials::new("key", "secret"),
    )
    .unwrap();
    config.tls_roots = TlsRoots::File(fixture(ca));
    S3Client::new(config)
}

/// The whole point: the four calls, over TLS, arriving as the same signed requests.
#[test]
fn the_four_calls_travel_over_tls() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    let client = client_for(&server, "ca-cert.pem");

    client.put("000007.sst", b"the sst bytes").unwrap();
    let got = client.get("000007.sst").unwrap();
    assert_eq!(got.body, b"the sst bytes");
    let listed = client.list("").unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    // The key on the wire is exactly the key the caller passed: `path_for` composes bucket and
    // key only, and the store prefix is the *caller's* to join (the tiered filesystem does it).
    assert_eq!(listed[0].key, "000007.sst");
    assert_eq!(listed[0].size, "the sst bytes".len() as u64);
    client.delete("000007.sst").unwrap();

    let seen = server.requests();
    let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["PUT", "GET", "GET", "DELETE"]);
    for request in &seen {
        assert!(
            request.authorization.starts_with("AWS4-HMAC-SHA256"),
            "every request is signed: {request:?}"
        );
        assert_eq!(
            request.host,
            format!("localhost:{}", server.address.port()),
            "the Host header is the endpoint's, and it is signed"
        );
    }
}

/// A ranged read, which is what a cold tiered block read actually is.
#[test]
fn a_ranged_get_over_tls_returns_the_range() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    let client = client_for(&server, "ca-cert.pem");

    client.put("block.sst", b"0123456789").unwrap();
    let got = client.get_range("block.sst", 2, 4, None).unwrap();
    assert_eq!(got.body, b"2345");
    assert_eq!(got.total_size, Some(10));
}

/// The ordinary case: the TLS pool keeps its connection across requests.
///
/// This one passes with either liveness check — a quiet server gives the naive peek nothing to
/// misread — so it is not the mechanism test it looks like. It is here because the ordinary case
/// is worth pinning anyway; `the_pool_survives_a_server_that_talks_between_requests` is the one
/// that can tell the two implementations apart.
#[test]
fn one_tls_connection_serves_many_requests() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    let client = client_for(&server, "ca-cert.pem");

    for i in 0..8 {
        client.put(&format!("{i}.sst"), b"body").unwrap();
        client.get(&format!("{i}.sst")).unwrap();
    }

    assert_eq!(
        server.connections_accepted(),
        1,
        "16 requests over one kept-alive TLS session (ADR 0039), not one handshake each"
    );
}

/// The pool survives a server that talks between requests, which is what TLS 1.3 servers do.
///
/// **This is the test that distinguishes the two liveness checks**, and the reason the plain
/// transport's could not simply be copied. The server sends a post-handshake message after every
/// response — a key update here; a real one sends session tickets — so the client comes back to a
/// pooled connection with bytes waiting on its socket. A check that reads "readable" as "not at a
/// message boundary" discards the connection every time: every response stays correct, every
/// request pays for a fresh handshake, and nothing but the connection count shows it.
///
/// Verified to fail against that naive check before being kept: with `absorb_pending` replaced by
/// a bare TCP peek this asserts 1 and gets 8.
#[test]
fn the_pool_survives_a_server_that_talks_between_requests() {
    let server = MockS3::chatty("localhost-cert.pem", "localhost-key.pem");
    let client = client_for(&server, "ca-cert.pem");

    for i in 0..8 {
        client.put(&format!("{i}.sst"), b"body").unwrap();
        // The post-handshake message is in flight while this client is idle; give it a moment to
        // arrive, which is exactly the race a real server creates and a fast loop hides.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert_eq!(
        server.connections_accepted(),
        1,
        "a key update between requests must not cost the pooled session"
    );
}

/// A certificate signed by a CA the client does not trust is refused, and **not retryable**.
#[test]
fn an_untrusted_certificate_is_refused_and_is_not_retryable() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    // The client trusts a CA that signed nothing here.
    let client = client_for(&server, "other-ca-cert.pem");

    let Err(error) = client.put("k.sst", b"body") else {
        panic!("a server whose chain does not reach a trusted root must be refused");
    };
    assert!(
        !error.is_retryable(),
        "a wrong trust store is a standing misconfiguration, not something to wait out: {error}"
    );
    assert!(
        error.to_string().to_lowercase().contains("certificate"),
        "the message says what was wrong: {error}"
    );
}

/// A certificate that does not cover the endpoint's name is refused the same way.
///
/// The chain is fine — the same CA signed it — and only the name is wrong, which is exactly the
/// case a client that skipped name verification would sail through.
#[test]
fn a_certificate_for_another_name_is_refused() {
    let server = MockS3::start("wrong-name-cert.pem", "wrong-name-key.pem");
    let client = client_for(&server, "ca-cert.pem");

    let Err(error) = client.put("k.sst", b"body") else {
        panic!("a certificate that does not name this endpoint must be refused");
    };
    assert!(!error.is_retryable(), "{error}");
}

/// An `https://` URL pointed at a server speaking plain HTTP fails as TLS, not as a bad response.
#[test]
fn plain_http_behind_an_https_url_is_a_tls_error() {
    // A listener that answers every connection with an HTTP response and no TLS at all.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut socket) = incoming else { return };
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });

    let endpoint = Endpoint::parse(&format!("https://localhost:{}", address.port())).unwrap();
    let mut config = Config::from_store_url(
        "s3://bucket/prefix",
        endpoint,
        "us-east-1",
        Credentials::new("key", "secret"),
    )
    .unwrap();
    config.tls_roots = TlsRoots::File(fixture("ca-cert.pem"));
    let client = S3Client::new(config);

    let Err(error) = client.put("k.sst", b"body") else {
        panic!("a plaintext server behind an https URL must fail");
    };
    assert!(
        !error.is_retryable(),
        "a plaintext endpoint behind an https:// URL is a misconfiguration: {error}"
    );
}

/// A CA file that is not a CA file is a startup error from `open`, before any request.
#[test]
fn open_reports_a_bad_trust_store_before_the_first_request() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    let endpoint = Endpoint::parse(&server.url()).unwrap();
    let mut config = Config::from_store_url(
        "s3://bucket/prefix",
        endpoint,
        "us-east-1",
        Credentials::new("key", "secret"),
    )
    .unwrap();
    config.tls_roots = TlsRoots::File(fixture("localhost-key.pem")); // a key, not a bundle

    let Err(error) = S3Client::open(config) else {
        panic!("open must refuse a trust store it cannot read");
    };
    assert!(error.to_string().contains("CERTIFICATE"), "{error}");
    assert!(!error.is_retryable(), "{error}");
}

/// The transport is reachable as a `Transport` like any other, which is what keeps the tier's
/// wiring unchanged.
#[test]
fn the_tls_transport_is_just_another_transport() {
    let server = MockS3::start("localhost-cert.pem", "localhost-key.pem");
    let transport = esker_s3::tls::TlsTransport::prepared(
        TlsRoots::File(fixture("ca-cert.pem")),
        esker_s3::transport::Timeouts::default(),
    )
    .unwrap();
    let response = transport
        .round_trip(
            "localhost",
            server.address.port(),
            b"GET /bucket/nothing HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
    assert_eq!(response.status, 404);
}
