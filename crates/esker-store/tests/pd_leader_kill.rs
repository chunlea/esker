//! A client whose placement driver was **killed** finds another member.
//!
//! # Why this is not the redirect the client already followed
//!
//! `RemotePd` learned to follow a `PdNotLeader` hint: the member it was talking to said *"not me,
//! try that one"*, and it moved. Every test of that path has three members answering, and the
//! leadership moving between them while all three are alive.
//!
//! **A killed member says nothing at all.** It does not answer `PdNotLeader`; it does not answer.
//! The client's socket closes, the next call cannot connect, and a client that only moved on a
//! refusal has nothing to move it — so it re-dials the corpse on its own cadence, for ever, while
//! two live members sit in its list with a leader between them.
//!
//! That is the case
//! [ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)'s
//! acceptance is about — *kill the placement driver that was leading, and a statement still
//! returns* — and it is the one the redirect rules do not reach. It is also
//! [debt #52](../../../docs/plans/debts-v1.1.md) one layer up: a client that never redialled a
//! **store** that came back cost 111 seconds of unavailability, and the shape here is its twin —
//! a client that never looks past a member that went away.
//!
//! # What makes the move safe
//!
//! Only a failure that provably **never left this process** moves the client:
//! `ProtoError::NotSent`, which is what a connection that could not be built is. A call that went
//! out and lost its answer is `Closed` or `Timeout`, and those are returned to the caller as they
//! were — repeating such a request elsewhere would be repeating a request that may have applied.
//! So the cost of a kill is at most the one call that was in flight when the socket died; the
//! next call, which has to build a connection, moves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_proto::{
    BoxFuture, PdMemberInfo, PdMembership, PdReq, PdResp, PdRole, ProtoError, Reply, Request,
    Response, Server, ServerHandle, Service,
};
use esker_store::RemotePd;
use esker_store::pd::PdClient;

/// A placement driver that answers `AllocId` and `Members`, and nothing else.
///
/// `esker-store` does not depend on `esker-pd` and must not — they are peers that meet on the wire
/// — so the driver here is a stand-in behind the real framing, exactly as `tests/pd_client.rs`
/// argues. What is under test is which member the client talks to, and that needs a member that
/// answers and a member that does not, not a Raft group.
#[derive(Debug)]
struct StandIn {
    id: u64,
    members: Vec<(u64, String)>,
}

impl StandIn {
    fn membership(&self) -> PdMembership {
        PdMembership {
            group_id: 0x5151_5151_5151_5151,
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
            let answer = match request {
                // The member id, so a caller can tell which one answered.
                PdReq::AllocId { count } => PdResp::AllocId {
                    start: self.id,
                    count,
                },
                PdReq::Members => PdResp::Members(self.membership()),
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

async fn serve(id: u64) -> (ServerHandle, std::net::SocketAddr) {
    // Bound first so the member list can name both before either answers, which is what a real
    // group's `--peers` does.
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::new(StandIn {
            id,
            members: Vec::new(),
        }) as Arc<dyn Service>,
        esker_proto::TransportConfig::new(),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    (server.spawn().unwrap(), address)
}

/// **The acceptance, at the store's layer.** Two members; the client believes the first; the first
/// is killed. The next call must be answered by the second.
///
/// Red before rule five: every call after the kill fails to connect and the client never looks
/// past the member it believed.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_whose_member_was_killed_is_answered_by_another() {
    let (first, first_address) = serve(1).await;
    let (second, second_address) = serve(2).await;

    let client = tokio::task::spawn_blocking(move || {
        let client =
            RemotePd::connect_to(&[first_address, second_address], Default::default()).unwrap();
        assert_eq!(
            client.alloc_id(1).unwrap(),
            1,
            "the client starts at the first member it was given"
        );
        client
    })
    .await
    .unwrap();

    // The kill. `shutdown` closes the listener and the connections it accepted, which is what a
    // killed process leaves behind on the client's side.
    first.shutdown().await.unwrap();

    let answered_by = tokio::task::spawn_blocking(move || {
        // Two calls, because the *first* may be the one that was in flight when the socket died —
        // an ambiguous failure this deliberately does not retry elsewhere. The second has to build
        // a connection, and that is the failure that moves the client.
        let mut last = Err(ProtoError::internal("never called"));
        for _ in 0..2 {
            last = client.alloc_id(1);
            if last.is_ok() {
                break;
            }
        }
        (last, client.address())
    })
    .await
    .unwrap();

    let (answer, believed) = answered_by;
    let id = answer.expect(
        "every call after the kill failed: the client sat on the member that went away while a \
         live one was in its list",
    );
    assert_eq!(id, 2, "the second member answered");
    assert_eq!(
        believed, second_address,
        "and the client stayed there rather than going back to the corpse on the next call"
    );

    second.shutdown().await.unwrap();
}

/// The other half: when **no** member can be reached, the failure is still a failure. A client
/// that advanced for ever would turn an outage into a hang.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_can_reach_nobody_fails_rather_than_spinning() {
    let (first, first_address) = serve(1).await;
    let (second, second_address) = serve(2).await;
    let client = tokio::task::spawn_blocking(move || {
        RemotePd::connect_to(&[first_address, second_address], Default::default()).unwrap()
    })
    .await
    .unwrap();
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();

    let began = std::time::Instant::now();
    let answer = tokio::task::spawn_blocking(move || client.alloc_id(1))
        .await
        .unwrap();
    assert!(answer.is_err(), "nobody was there to answer");
    assert!(
        began.elapsed() < std::time::Duration::from_secs(20),
        "the client spun instead of giving up: {:?}",
        began.elapsed()
    );
}
