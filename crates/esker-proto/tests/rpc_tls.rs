//! The framed RPC over TLS, and over mTLS, against a real server in this process.
//!
//! What these are for, in order of what would hurt most if it were wrong:
//!
//! * **the same requests arrive.** TLS is a wrapper, and the framing, the `Hello` negotiation and
//!   the demultiplexer must be exactly what they are in the clear. A `Ping` that round-trips
//!   through a session proves the whole stack, not just the handshake.
//! * **mTLS turns a peer away.** This is the reason the RPC surface wanted client certificates at
//!   all: a store↔store or PD↔PD link where both ends are ours. A node whose certificate a
//!   different CA signed must not get a connection, and a node with **no** certificate must not
//!   either — the second is the one a server that merely *requested* rather than *required* client
//!   auth would let through, and nothing in the traffic would show it.
//! * **a plaintext peer is refused when TLS is on.** This protocol has no in-band upgrade, so
//!   "required" is the only setting there is, and a clear connection must fail rather than be
//!   served.
//!
//! The certificates beside this file are throwaway: see `tests/fixtures/README.md`.

#![cfg(feature = "tls")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use esker_proto::transport::{RpcTls, Server, TcpTransport, Transport, TransportConfig};
use esker_proto::{ProtoError, RaftBatch, Request, Response};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// This node's identity, and the roots it trusts.
fn tls_for(node: &str, roots: &str, mutual: bool) -> RpcTls {
    RpcTls::from_files(
        &fixture(&format!("{node}-cert.pem")),
        &fixture(&format!("{node}-key.pem")),
        &fixture(roots),
        mutual,
    )
    .unwrap_or_else(|error| panic!("building {node}'s TLS configuration: {error}"))
}

/// A service that acknowledges anything — enough to prove a request crossed the session.
///
/// An empty Raft batch is the smallest request/response pair the protocol has, and it is the
/// traffic this surface actually carries between stores and between placement drivers.
#[derive(Debug)]
struct Ack;

impl esker_proto::transport::Service for Ack {
    fn call(
        &self,
        _request: Request,
    ) -> esker_proto::transport::BoxFuture<'_, Result<esker_proto::transport::Reply, ProtoError>>
    {
        Box::pin(async move { Ok(esker_proto::transport::Reply::Unary(Response::Raft)) })
    }

    fn store_id(&self) -> u64 {
        7
    }
}

/// One empty Raft batch: the smallest thing that exercises framing, Hello and the demultiplexer.
fn a_request() -> Request {
    Request::Raft(RaftBatch::new(Vec::new()))
}

/// Starts a server on an ephemeral port and answers with its address.
async fn serve(tls: RpcTls) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, Arc::new(Ack), TransportConfig::new())
        .unwrap()
        .with_tls(tls);
    tokio::spawn(async move {
        let _ = server.serve(std::future::pending::<()>()).await;
    });
    // The listener is already bound, so a connect cannot race the bind; the task only has to
    // reach its accept loop, which the first connect attempt waits for anyway.
    address
}

/// One request, over TLS, arriving as the same framed request it is in the clear.
#[tokio::test]
async fn a_request_crosses_a_tls_connection() {
    let address = serve(tls_for("node-a", "ca-cert.pem", false)).await;
    let client = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", false),
        Some("localhost"),
    )
    .await
    .expect("a client that trusts the CA connects");

    let response = client.call(a_request()).await.expect("the call answers");
    assert!(matches!(response, Response::Raft));
}

/// The certificate is checked against the **address's IP** when no name is given, which is what an
/// address book of `SocketAddr`s can offer.
#[tokio::test]
async fn a_peer_is_verified_against_its_address_when_no_name_is_given() {
    let address = serve(tls_for("node-a", "ca-cert.pem", false)).await;
    let client = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", false),
        None,
    )
    .await
    .expect("the fixture carries an IP SAN for 127.0.0.1");
    assert!(matches!(
        client.call(a_request()).await.unwrap(),
        Response::Raft
    ));
}

/// mTLS: two nodes that share a CA reach each other.
#[tokio::test]
async fn mutual_tls_lets_two_nodes_of_the_same_cluster_talk() {
    let address = serve(tls_for("node-a", "ca-cert.pem", true)).await;
    let client = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", true),
        Some("localhost"),
    )
    .await
    .expect("node-b presents a certificate node-a's CA signed");
    assert!(matches!(
        client.call(a_request()).await.unwrap(),
        Response::Raft
    ));
}

/// **The test mTLS exists for**: a peer whose certificate a different CA signed is turned away.
///
/// Its own chain is perfectly valid — `other-ca-cert.pem` signed it — and that is the point: what
/// disqualifies it is that *this* cluster's CA did not.
#[tokio::test]
async fn mutual_tls_turns_away_a_peer_from_another_ca() {
    let address = serve(tls_for("node-a", "ca-cert.pem", true)).await;
    let stranger = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        // The stranger trusts our CA, so it is happy with the server; the server is not happy
        // with it, which is the direction under test.
        &tls_for("stranger", "ca-cert.pem", true),
        Some("localhost"),
    )
    .await;
    assert!(
        stranger.is_err(),
        "a certificate from an unrelated CA must not open a cluster connection"
    );
}

/// The case a server that *requested* rather than *required* client certificates would let
/// through: a peer with no certificate at all.
#[tokio::test]
async fn mutual_tls_turns_away_a_peer_with_no_certificate() {
    let address = serve(tls_for("node-a", "ca-cert.pem", true)).await;
    // A client configured without mutual auth presents nothing.
    let anonymous = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", false),
        Some("localhost"),
    )
    .await;
    assert!(
        anonymous.is_err(),
        "mTLS must require a certificate, not merely request one"
    );
}

/// A server that is not doing mTLS still serves a client that has no certificate — which is the
/// client↔store link, where the other end is a SQL node with no cluster identity.
#[tokio::test]
async fn one_way_tls_serves_a_client_without_a_certificate() {
    let address = serve(tls_for("node-a", "ca-cert.pem", false)).await;
    let client = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", false),
        Some("localhost"),
    )
    .await
    .expect("a one-way server asks for nothing");
    assert!(matches!(
        client.call(a_request()).await.unwrap(),
        Response::Raft
    ));
}

/// A plaintext client against a TLS server fails, rather than being served in the clear.
///
/// There is no `SSLRequest` here — this protocol has no in-band upgrade — so "TLS is on" can only
/// mean "required", and a clear connection has to fail. It fails at the `Hello`, because the
/// server sees a handshake that is not one.
#[tokio::test]
async fn a_plaintext_peer_cannot_reach_a_tls_server() {
    let address = serve(tls_for("node-a", "ca-cert.pem", false)).await;
    let plain = TcpTransport::connect_with(address, TransportConfig::new()).await;
    assert!(
        plain.is_err(),
        "a server with TLS on must not serve a peer that never handshook"
    );
}

/// And the other way round: a TLS client against a plaintext server.
#[tokio::test]
async fn a_tls_peer_cannot_reach_a_plaintext_server() {
    let address = serve(RpcTls::disabled()).await;
    let encrypted = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", false),
        Some("localhost"),
    )
    .await;
    assert!(
        encrypted.is_err(),
        "a client that requires TLS must not fall back to the clear"
    );
}

/// How a rejected peer *learns* it was rejected, which is not what one would guess.
///
/// TLS 1.3 lets a client finish its side of the handshake before the server has validated the
/// certificate it just sent — so under mTLS the client's own handshake **succeeds**, and the
/// refusal arrives afterwards as a closed connection during the `Hello` exchange. It is therefore
/// `Closed`, not a handshake error, and an operator debugging "why will this node not join"
/// will see a peer that closed rather than a certificate complaint. The complaint is on the
/// *server's* side, which is where the log line naming the CA appears.
///
/// Either error is honest about the one thing a caller must not get wrong: nothing was sent, so
/// nothing was applied. This asserts that much and records which one it is today, so that a change
/// making it the more precise `NotSent` is a deliberate improvement rather than an accident.
#[tokio::test]
async fn a_rejected_peer_learns_of_it_as_a_closed_connection() {
    let address = serve(tls_for("node-a", "ca-cert.pem", true)).await;
    let Err(error) = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("stranger", "ca-cert.pem", true),
        Some("localhost"),
    )
    .await
    else {
        panic!("the stranger must be refused");
    };
    assert!(
        matches!(
            error,
            ProtoError::Closed { .. } | ProtoError::NotSent { .. }
        ),
        "no request was sent, so the outcome must be one of the two that say so: {error:?}"
    );
}

/// Many requests over one TLS connection, which is what a Raft link actually does.
#[tokio::test]
async fn many_requests_share_one_tls_connection() {
    let address = serve(tls_for("node-a", "ca-cert.pem", true)).await;
    let client = TcpTransport::connect_with_tls(
        address,
        TransportConfig::new(),
        &tls_for("node-b", "ca-cert.pem", true),
        Some("localhost"),
    )
    .await
    .unwrap();

    for _ in 0..64 {
        assert!(matches!(
            client.call(a_request()).await.unwrap(),
            Response::Raft
        ));
    }
}
