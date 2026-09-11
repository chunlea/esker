//! A SQL node whose placement driver moved, or died, keeps answering.
//!
//! # What a SQL node loses with its driver
//!
//! [`PdConn`] is four things at once: the node's [`TimestampOracle`], its
//! [`RegionResolver`](esker_client::region_cache::RegionResolver), its schema lease and its
//! columnar report. **Every statement starts by asking it for a timestamp**, so a node that cannot
//! reach a placement driver runs nothing at all — not even a `SELECT` — and every session sees
//! `08006` (`esker-coord/h1-driver-kill.md` §2).
//!
//! Until [ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)
//! that was unavoidable, because there was only ever one driver. It is now avoidable and this file
//! is what says so: a group of members, a leader that moves or dies, and a timestamp that still
//! arrives.
//!
//! # Why stand-ins rather than `esker-pd`
//!
//! What is under test is **which member this node talks to**, and that needs a member that
//! redirects and a member that answers — not an election. A real group would elect whoever it
//! liked and the test would have to chase it; these two say exactly what the wire says, which is
//! the only thing `PdConn` can act on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_client::TimestampOracle;
use esker_proto::{
    BoxFuture, PdMemberInfo, PdMembership, PdReq, PdResp, PdRole, ProtoError, Reply, Request,
    Response, Server, ServerHandle, Service, TransportConfig,
};
use esker_sql::pd::PdConn;

const GROUP: u64 = 0x0108_0108_0108_0108;

/// A member of a placement-driver group that either answers or redirects.
#[derive(Debug)]
struct StandIn {
    /// This member's id, and the first timestamp it hands out — so a caller can tell from the
    /// answer alone which member served it.
    id: u64,
    /// Where to send a caller instead, or `None` to answer.
    redirect_to: Option<String>,
    /// Every member, as this one sees the group.
    members: Vec<(u64, String)>,
}

impl StandIn {
    fn membership(&self) -> PdMembership {
        PdMembership {
            group_id: GROUP,
            this_id: self.id,
            leader_id: self.id,
            term: 1,
            members: self
                .members
                .iter()
                .map(|(id, address)| PdMemberInfo {
                    id: *id,
                    address: address.clone(),
                    role: PdRole::Voter,
                })
                .collect(),
        }
    }
}

impl Service for StandIn {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        Box::pin(async move {
            let Request::Pd { request, .. } = request else {
                return Err(ProtoError::invalid(
                    "this stand-in answers only Pd requests",
                ));
            };
            // `Members` is answered by every member, leader or not — an operator asks it exactly
            // when the leader is the thing that is missing, and so does a client chasing a hint.
            if matches!(request, PdReq::Members) {
                return Ok(Reply::Unary(Response::Pd(PdResp::Members(
                    self.membership(),
                ))));
            }
            if let Some(address) = &self.redirect_to {
                return Err(ProtoError::PdNotLeader {
                    leader_id: 0,
                    leader_address: address.clone(),
                });
            }
            let answer = match request {
                PdReq::Tso { count } => PdResp::Tso {
                    start_ts: self.id,
                    count,
                },
                other => {
                    return Err(ProtoError::invalid(format!(
                        "this stand-in does not answer {}",
                        other.method().name()
                    )));
                }
            };
            Ok(Reply::Unary(Response::Pd(answer)))
        })
    }

    fn store_id(&self) -> u64 {
        0
    }
}

/// Binds a member without serving it yet, so both addresses are known before either answers —
/// which is what a real group's `--peers` list needs.
fn bind() -> (std::net::TcpListener, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn serve(
    listener: std::net::TcpListener,
    id: u64,
    redirect_to: Option<String>,
    members: Vec<(u64, String)>,
) -> ServerHandle {
    let server = Server::from_listener(
        listener,
        Arc::new(StandIn {
            id,
            redirect_to,
            members,
        }) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .unwrap();
    server.spawn().unwrap()
}

/// **A leader that moved.** The member this node was given first refuses and names the second; the
/// timestamp comes from the second, and the node stays there.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_follows_the_member_its_driver_names() {
    let (first_socket, first) = bind();
    let (second_socket, second) = bind();
    let group = vec![(1, first.to_string()), (2, second.to_string())];
    let a = serve(first_socket, 1, Some(second.to_string()), group.clone()).await;
    let b = serve(second_socket, 2, None, group).await;

    let believed = tokio::task::spawn_blocking(move || {
        let conn = PdConn::to_group(&[first, second], TransportConfig::new()).unwrap();
        let start = conn.tso(1).expect(
            "the node could not get a timestamp: the member it was given is not the leader and it \
             did not follow the hint. Every statement starts here, so this is a node that runs \
             nothing at all.",
        );
        assert_eq!(start, 2, "the second member answered");
        conn.address()
    })
    .await
    .unwrap();
    assert_eq!(
        believed, second,
        "the node went back to the member that refuses, so every statement pays the redirect"
    );

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

/// **A leader that was killed**, which is ADR 0108's acceptance: no refusal, no hint, no answer at
/// all. The node has to look past the member that went away on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_whose_driver_was_killed_still_gets_a_timestamp() {
    let (first_socket, first) = bind();
    let (second_socket, second) = bind();
    let group = vec![(1, first.to_string()), (2, second.to_string())];
    let a = serve(first_socket, 1, None, group.clone()).await;
    let b = serve(second_socket, 2, None, group).await;

    let conn = tokio::task::spawn_blocking(move || {
        let conn = PdConn::to_group(&[first, second], TransportConfig::new()).unwrap();
        assert_eq!(
            conn.tso(1).unwrap(),
            1,
            "the node starts at the first member"
        );
        conn
    })
    .await
    .unwrap();

    // The kill.
    a.shutdown().await.unwrap();

    let (answer, believed) = tokio::task::spawn_blocking(move || {
        // Two, because the first may be the call that was in flight when the socket died — an
        // ambiguous failure this deliberately does not repeat elsewhere. The next has to build a
        // connection, and that is the failure that moves the node.
        let mut last = Err(ProtoError::internal("never called"));
        for _ in 0..2 {
            last = conn.tso(1);
            if last.is_ok() {
                break;
            }
        }
        (last, conn.address())
    })
    .await
    .unwrap();

    assert_eq!(
        answer.expect(
            "every call after the kill failed: the node sat on the driver that went away while a \
             live one was in its list, so no statement could start"
        ),
        2,
        "the surviving member answered"
    );
    assert_eq!(believed, second, "and the node stayed there");

    b.shutdown().await.unwrap();
}
