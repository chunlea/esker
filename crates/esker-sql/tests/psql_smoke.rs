//! The listener, driven end to end — over a pipe always, and by a real `psql` when there is one.
//!
//! Two layers on purpose. The pipe tests need nothing installed and so they run everywhere: they
//! drive [`Connection`] over `tokio::io::duplex` and assert on the exact bytes of the startup
//! handshake, including the protocol-3.2 downgrade that decides whether a current `libpq` can
//! connect at all.
//!
//! The `psql` test is the one that proves it against software we did not write, and it **skips**
//! rather than fails when `psql` is absent, because a missing client is a fact about the machine
//! and not a defect in the server. When it does run it is the only test in the crate that exercises
//! a real client's own idea of the protocol.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use esker_sql::pgwire::server::{Auth, Config, Connection, Executors, NotYetExecuting, serve_on};
use esker_sql::pgwire::session::Execute;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Hands every session the placeholder executor: the executor itself is unit 6.
struct Sessions;

impl Executors for Sessions {
    fn for_session(&self) -> Box<dyn Execute + Send> {
        Box::new(NotYetExecuting)
    }
}

/// A startup packet asking for protocol 3.`minor`.
fn startup_packet(minor: u16, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(0x0003_0000u32 | u32::from(minor)).to_be_bytes());
    for (name, value) in [("user", "esker"), ("database", "esker")]
        .iter()
        .chain(extra.iter())
    {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut packet = u32::try_from(body.len() + 4).unwrap().to_be_bytes().to_vec();
    packet.extend_from_slice(&body);
    packet
}

/// Reads until a `ReadyForQuery` has arrived, or gives up rather than hanging the suite.
async fn read_until_ready(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let deadline = Duration::from_secs(5);
    tokio::time::timeout(deadline, async {
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if frames(&buffer).iter().any(|(tag, _)| *tag == 'Z') {
                return;
            }
        }
    })
    .await
    .expect("the server did not answer within five seconds");
    buffer
}

/// Splits a reply into `(tag, body)`.
fn frames(bytes: &[u8]) -> Vec<(char, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 5 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
                as usize;
        if at + 1 + length > bytes.len() {
            break;
        }
        out.push((bytes[at] as char, bytes[at + 5..at + 1 + length].to_vec()));
        at += 1 + length;
    }
    out
}

fn tags(bytes: &[u8]) -> String {
    frames(bytes).into_iter().map(|(tag, _)| tag).collect()
}

/// Runs a connection over a pipe, feeding it `input` and returning everything it wrote.
async fn over_a_pipe(input: Vec<u8>) -> Vec<u8> {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(Box::new(NotYetExecuting)).await;
    });
    client.write_all(&input).await.unwrap();
    client.flush().await.unwrap();
    read_until_ready(&mut client).await
}

#[tokio::test]
async fn a_startup_handshake_completes_and_reports_readiness() {
    let reply = over_a_pipe(startup_packet(0, &[])).await;
    let tags = tags(&reply);
    assert!(
        tags.starts_with('R'),
        "authentication comes first, got {tags}"
    );
    assert!(tags.ends_with('Z'), "readiness comes last, got {tags}");
    assert!(tags.contains('K'), "the cancel key is part of startup");
    assert!(
        tags.matches('S').count() >= 4,
        "the client is owed the settings it branches on, got {tags}"
    );
    // Idle: no transaction is open on a fresh connection.
    let ready = frames(&reply).pop().unwrap();
    assert_eq!(ready.1, b"I");
}

/// The directive that started this unit, over a socket rather than against a golden: a client
/// asking for 3.2 is downgraded and connects, never refused.
#[tokio::test]
async fn a_client_asking_for_protocol_3_2_is_downgraded_and_still_connects() {
    let reply = over_a_pipe(startup_packet(2, &[])).await;
    let framed = frames(&reply);
    assert_eq!(
        framed[0].0, 'v',
        "NegotiateProtocolVersion comes first, before the ordinary sequence"
    );
    // 3.0, and no unsupported `_pq_` options.
    assert_eq!(framed[0].1[..4], [0, 3, 0, 0]);
    assert_eq!(framed[0].1[4..8], [0, 0, 0, 0]);
    assert_eq!(framed[1].0, 'R', "and then startup carries on as normal");
    assert!(tags(&reply).ends_with('Z'), "the client is let in");
}

/// An unknown `_pq_.` protocol option is reported by name in the same message, and is not fatal.
#[tokio::test]
async fn an_unknown_protocol_option_is_named_and_the_client_still_connects() {
    let reply = over_a_pipe(startup_packet(0, &[("_pq_.made_up", "on")])).await;
    let framed = frames(&reply);
    assert_eq!(framed[0].0, 'v');
    assert_eq!(framed[0].1[4..8], [0, 0, 0, 1], "one option was refused");
    assert!(
        framed[0].1.ends_with(b"_pq_.made_up\0"),
        "and it is named so the client knows which"
    );
    assert!(tags(&reply).ends_with('Z'));
}

/// `SSLRequest` is answered with a single bare byte — not a framed message — and the client then
/// sends its real startup packet on the same connection.
#[tokio::test]
async fn an_ssl_request_is_refused_and_the_connection_continues() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(Box::new(NotYetExecuting)).await;
    });

    let mut ssl_request = 8u32.to_be_bytes().to_vec();
    ssl_request.extend_from_slice(&80_877_103u32.to_be_bytes());
    client.write_all(&ssl_request).await.unwrap();
    client.flush().await.unwrap();

    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"N", "a bare N, with no length and no tag");

    client.write_all(&startup_packet(0, &[])).await.unwrap();
    client.flush().await.unwrap();
    assert!(tags(&read_until_ready(&mut client).await).ends_with('Z'));
}

/// Contract C2 over a real connection: a statement is refused by name, and the session survives to
/// answer the next one.
#[tokio::test]
async fn a_statement_is_refused_by_name_and_the_session_carries_on() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(Box::new(NotYetExecuting)).await;
    });
    client.write_all(&startup_packet(0, &[])).await.unwrap();
    read_until_ready(&mut client).await;

    let mut query = vec![b'Q'];
    let body = b"SELECT 1\0";
    query.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    query.extend_from_slice(body);
    client.write_all(&query).await.unwrap();

    let reply = read_until_ready(&mut client).await;
    assert_eq!(tags(&reply), "EZ");
    let error = &frames(&reply)[0].1;
    assert!(
        error.windows(7).any(|w| w == b"C0A000\0"),
        "the refusal carries 0A000"
    );
    assert_eq!(frames(&reply)[1].1, b"I", "and the session is idle, not stuck");
}

// --- and the same thing, against software we did not write ------------------------------------

/// True when a `psql` is on this machine to test with.
fn psql_available() -> bool {
    std::process::Command::new("psql")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A real `psql`, connecting to a real listener.
///
/// Everything else in this crate tests our reading of the protocol against our own writing of it.
/// This is the only test where the other end was written by someone else, which is the only way to
/// find out that the two readings agree. It skips when `psql` is absent: that is a fact about the
/// machine, not a defect in the server.
#[tokio::test(flavor = "multi_thread")]
async fn psql_connects_and_is_told_the_truth() {
    if !psql_available() {
        eprintln!("skipping: no psql on this machine");
        return;
    }

    // Port 0, then read back what the operating system chose, so the test cannot collide with
    // anything else on the machine or with another copy of itself.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_on(
            listener,
            Config {
                address: address.to_string(),
                auth: Auth::Trust,
                ..Config::default()
            },
            Arc::new(Sessions),
        )
        .await;
    });

    let url = format!(
        "postgresql://esker@127.0.0.1:{}/esker?sslmode=disable",
        address.port()
    );
    let run = |args: Vec<String>| {
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("psql")
                .arg(&url)
                .arg("-X")
                .arg("-v")
                .arg("VERBOSITY=verbose")
                .args(&args)
                .env("PGCONNECT_TIMEOUT", "5")
                .output()
                .expect("psql should run")
        })
    };

    // Connecting at all is the thing being tested: startup, authentication, ReadyForQuery.
    let output = run(vec!["-c".into(), "SELECT 1".into()]).await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0A000"),
        "psql connected and should have been refused by name; stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("could not connect") && !stderr.contains("server closed"),
        "the connection itself must succeed; stderr was: {stderr}"
    );

    // The transaction state machine, driven by a real client: an error inside a block poisons it
    // until the block ends, and `psql` reports the 25P02 for the statement in between.
    let output = run(vec![
        "-c".into(),
        "BEGIN".into(),
        "-c".into(),
        "SELECT 1".into(),
        "-c".into(),
        "SELECT 2".into(),
        "-c".into(),
        "COMMIT".into(),
    ])
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("25P02"),
        "the statement after a failure inside a block must be refused with 25P02: {stderr}"
    );

    // And the protocol-3.2 path, with a real libpq asking for it rather than a golden.
    let latest = format!(
        "postgresql://esker@127.0.0.1:{}/esker?sslmode=disable&max_protocol_version=latest",
        address.port()
    );
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("psql")
            .arg(&latest)
            .arg("-X")
            .arg("-c")
            .arg("SELECT 1")
            .env("PGCONNECT_TIMEOUT", "5")
            .output()
            .expect("psql should run")
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") && !stderr.contains("could not connect"),
        "a client asking for protocol 3.2 must be downgraded and let in; stderr was: {stderr}"
    );
}
