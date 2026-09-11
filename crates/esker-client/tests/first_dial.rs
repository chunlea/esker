//! A store this client has **never** reached is reachable once it comes up.
//!
//! # The hole, and what it cost
//!
//! `TcpStores` keys its book by the store id a **handshake** reports, which is the right key: it
//! is what makes a `NotLeader` redirect land on the store the peer list names rather than on
//! whoever happens to hold an address. But a store that was down when the book was built answered
//! no handshake, so it reported no id, so it has no key — and a client that cannot name a store
//! cannot dial it however many times it restarts. The comment on `connect_all` has said so since
//! #52 closed the other half:
//!
//! > **A store that is down right now is missing from this book for ever**, because a book keyed
//! > by the store id its handshake reported has no key for an address that never answered.
//!
//! run 124 is what that costs. `esker durability chaos` withheld **26 of 28 rounds** over
//! twenty-six minutes — every round from 180 s to the end — with one message:
//!
//! ```text
//! no kill: the cluster could not acknowledge a write
//!          (request not sent: no address is known for store 4)
//! ```
//!
//! Its client had never needed store 4 until the leader moved there, and from then on it could not
//! name it. The SQL node on the same cluster ran the whole file clean throughout, so this is not a
//! cluster that was down — it is one client that had lost the ability to ask. **The run could not
//! measure its own headline claim**, which was a store dying every sixty seconds for
//! twenty-four minutes: two kills landed out of twenty-eight.
//!
//! # What this asserts
//!
//! The narrow half, which is the one run 124 hit: an address the client **was given** and that did
//! not answer at construction is kept, and tried again when something asks for a store the book
//! cannot name. Not the wider half — a store whose address the client was never given at all — for
//! which the answer is still PD's store list and which `TODO(debt-c6 #4)` still names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::TcpStores;
use esker_client::transport::StoreTransport;
use esker_proto::{RawKvReq, Request, RequestHeader, TransportConfig};

/// The store that is up the whole time, so the book is not empty and `connect_all` succeeds.
const PRESENT: u64 = 1;
/// The store that is down when the client is built and comes up afterwards.
const LATE: u64 = 2;

/// A store on `listener`, serving until its handle is dropped.
#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn serve(
    listener: TcpListener,
    store_id: u64,
    dir: &std::path::Path,
) -> esker_proto::ServerHandle {
    let store = esker_store::Store::open(
        dir,
        esker_store::StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: 1,
            ..esker_store::StoreOptions::new()
        },
    )
    .expect("the store opens");
    let service: Arc<dyn esker_proto::Service> = esker_store::StoreService::new(store);
    esker_proto::Server::from_listener(listener, service, TransportConfig::new())
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
}

/// One `RawKv::Get` at `store_id` — enough to prove the socket carried a request and an answer.
fn ask(stores: &TcpStores, store_id: u64) -> Result<(), esker_proto::ProtoError> {
    let request = Request::RawKv {
        header: RequestHeader::new(1, esker_proto::Epoch::INITIAL, store_id),
        request: RawKvReq::Get {
            key: bytes::Bytes::from_static(b"k"),
        },
    };
    stores
        .call(store_id, &request, Instant::now() + Duration::from_secs(10))
        .map(|_| ())
}

/// **The whole test.** The client is given both addresses; only one answers; the other comes up
/// afterwards and must be reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_was_down_when_the_client_was_built_is_reachable_once_it_is_up() {
    let present_dir = tempfile::tempdir().unwrap();
    let late_dir = tempfile::tempdir().unwrap();

    // Both ports are bound **now**, so both addresses are real and neither can be taken by
    // somebody else between here and the server that will use it. Only one is served.
    let present_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let late_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let present_address = present_socket.local_addr().unwrap();
    let late_address = late_socket.local_addr().unwrap();

    let present = serve(present_socket, PRESENT, present_dir.path()).await;

    // The book: both addresses given, one of them refusing the handshake because nothing is
    // serving it yet. This is `bench_route::routed`'s shape — PD names the stores, the client
    // dials them all — and one of them being down is the ordinary case, not a rare one.
    let stores = tokio::task::spawn_blocking(move || {
        TcpStores::connect_all(&[present_address, late_address], TransportConfig::new())
            .expect("a book with one live store is still a book")
    })
    .await
    .unwrap();
    assert_eq!(
        stores.store_ids(),
        vec![PRESENT],
        "only the store that answered a handshake can be named yet"
    );

    // **And now the store comes up.** Nothing tells the client; nothing can — the client is the
    // thing that has to notice.
    let late = serve(late_socket, LATE, late_dir.path()).await;

    let answered = tokio::task::spawn_blocking(move || {
        // Two attempts, because the redial cadence is deliberately not zero: a client that
        // hammered `connect(2)` on every statement is what run 120 measured at 31,700 attempts a
        // second. One cadence of 100 ms is the whole of the wait.
        let mut last = ask(&stores, LATE);
        for _ in 0..20 {
            if last.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            last = ask(&stores, LATE);
        }
        (last, stores.store_ids())
    })
    .await
    .unwrap();

    let (result, ids) = answered;
    result.expect(
        "the client could not reach a store it was given the address of, because that store was \
         down when the book was built. This is run 124's `no address is known for store 4`: a \
         client that loses a store it has never dialled never gets it back, and every chaos round \
         after it is withheld.",
    );
    assert!(
        ids.contains(&LATE),
        "the store answered but was not learned, so the next call pays the discovery again: {ids:?}"
    );

    let _ = late.shutdown().await;
    let _ = present.shutdown().await;
}

/// The other half of the same rule: an address that names a **different** store than the one asked
/// for is not adopted under that name. The handshake is the key, and a client that took an address
/// on faith would route confidently to the wrong store — which is the mistake the key exists to
/// prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unclaimed_address_is_adopted_under_the_store_that_answers_it() {
    let present_dir = tempfile::tempdir().unwrap();
    let late_dir = tempfile::tempdir().unwrap();
    let present_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let late_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let present_address = present_socket.local_addr().unwrap();
    let late_address = late_socket.local_addr().unwrap();

    let present = serve(present_socket, PRESENT, present_dir.path()).await;
    let stores = tokio::task::spawn_blocking(move || {
        TcpStores::connect_all(&[present_address, late_address], TransportConfig::new()).unwrap()
    })
    .await
    .unwrap();

    // The address comes up as store 2 — and something asks for store **9**, which nothing in this
    // cluster is.
    let late = serve(late_socket, LATE, late_dir.path()).await;
    let (refused, ids) = tokio::task::spawn_blocking(move || {
        let mut refused = ask(&stores, 9);
        for _ in 0..10 {
            if refused.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            refused = ask(&stores, 9);
        }
        (refused, stores.store_ids())
    })
    .await
    .unwrap();

    assert!(
        refused.is_err(),
        "a request for a store nothing answers as was served by whoever held an address"
    );
    assert!(
        !ids.contains(&9),
        "store 9 was invented from an address that answers as something else: {ids:?}"
    );
    assert!(
        ids.contains(&LATE),
        "the address was dialled and its real store id was not learned from it: {ids:?}"
    );

    let _ = late.shutdown().await;
    let _ = present.shutdown().await;
}
