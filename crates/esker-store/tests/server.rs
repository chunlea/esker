//! The store over a real socket.
//!
//! Every test here binds `127.0.0.1:0`, serves a real `Store` on a real temporary directory,
//! and talks to it with a real `TcpTransport`. Nothing is stubbed: the bytes go through the
//! kernel, the engine writes to a disk, and an `fsync` really happens on the `sync = true`
//! path. The unit tests in the crate prove the handlers; these prove the whole path, which is
//! where a layering mistake — a prefix applied twice, a header dropped, a shutdown that closes
//! the database under a running request — would actually show up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::messages::Hello;
use esker_proto::{
    Epoch, Frame, FrameDecoder, FrameKind, MAX_FRAME_SIZE, ProtoError, RawKvReq, RawKvResp,
    Request, RequestHeader, RequestOutcome, Server, ServerHandle, TcpTransport, Transport,
    TransportConfig, WIRE_VERSION,
};
use esker_store::{Store, StoreOptions, StoreService};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A store serving on an ephemeral port, and the directory it lives in.
///
/// The directory is held here so it outlives the server. A test that reopens the database must
/// own it *past* the `Running`, which is why [`start_at`] exists.
struct Running {
    handle: ServerHandle,
    store: Arc<Store>,
    /// Kept only to delay the directory's deletion; never read.
    #[allow(dead_code)]
    dir: Option<TempDir>,
}

async fn start() -> Running {
    let dir = TempDir::new().unwrap();
    let mut running = start_at(dir.path(), TransportConfig::new()).await;
    running.dir = Some(dir);
    running
}

/// Serves a store on `path`, which the caller owns and keeps.
async fn start_at(path: &std::path::Path, config: TransportConfig) -> Running {
    let store = Store::open(path, StoreOptions::new()).unwrap();
    let handle = Server::bind("127.0.0.1:0", StoreService::new(Arc::clone(&store)), config)
        .await
        .unwrap()
        .spawn()
        .unwrap();
    Running {
        handle,
        store,
        dir: None,
    }
}

fn header() -> RequestHeader {
    // Peer 0: "no opinion about the leader", which is what a freshly connected client sends.
    RequestHeader::new(1, Epoch::INITIAL, 0)
}

async fn call(transport: &TcpTransport, request: RawKvReq) -> Result<RawKvResp, ProtoError> {
    transport
        .call(Request::raw_kv(header(), request))
        .await?
        .into_raw_kv()
}

/// Every `RawKv` method, over the wire, against a real engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_method_round_trips_over_tcp() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    assert_eq!(
        call(&transport, RawKvReq::get(&b"k"[..])).await.unwrap(),
        RawKvResp::Get { value: None }
    );
    assert_eq!(
        call(&transport, RawKvReq::put(&b"k"[..], &b"v"[..]))
            .await
            .unwrap(),
        RawKvResp::Put
    );
    assert_eq!(
        call(&transport, RawKvReq::get(&b"k"[..])).await.unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"v"))
        }
    );

    assert_eq!(
        call(
            &transport,
            RawKvReq::batch_put(vec![
                (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
            ]),
        )
        .await
        .unwrap(),
        RawKvResp::BatchPut
    );
    assert_eq!(
        call(
            &transport,
            RawKvReq::BatchGet {
                keys: vec![
                    Bytes::from_static(b"a"),
                    Bytes::from_static(b"nope"),
                    Bytes::from_static(b"b"),
                ],
            },
        )
        .await
        .unwrap(),
        RawKvResp::BatchGet {
            values: vec![
                Some(Bytes::from_static(b"1")),
                None,
                Some(Bytes::from_static(b"2")),
            ],
        }
    );

    let RawKvResp::Scan { pairs } = call(&transport, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap()
    else {
        panic!("not a scan response");
    };
    let keys: Vec<&[u8]> = pairs.iter().map(|(key, _)| &key[..]).collect();
    assert_eq!(keys, [b"a", b"b", b"k"]);

    assert_eq!(
        call(
            &transport,
            RawKvReq::compare_and_swap(
                &b"k"[..],
                Some(Bytes::from_static(b"v")),
                Some(Bytes::from_static(b"swapped")),
            ),
        )
        .await
        .unwrap(),
        RawKvResp::CompareAndSwap {
            swapped: true,
            previous: Some(Bytes::from_static(b"v")),
        }
    );

    assert_eq!(
        call(&transport, RawKvReq::delete_range(&b"a"[..], &b"c"[..]))
            .await
            .unwrap(),
        RawKvResp::DeleteRange { deleted: 2 }
    );
    assert_eq!(
        call(&transport, RawKvReq::delete(&b"k"[..])).await.unwrap(),
        RawKvResp::Delete
    );
    let RawKvResp::Scan { pairs } = call(&transport, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap()
    else {
        panic!("not a scan response");
    };
    assert!(pairs.is_empty(), "the store is not empty: {pairs:?}");

    running.handle.shutdown().await.unwrap();
}

/// The client sends raw user bytes; the `'r'` prefix never crosses the wire. A key that *is*
/// the namespace byte is the case that catches a prefix applied twice, or not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_namespace_prefix_never_crosses_the_wire() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    for key in [&b"r"[..], b"", b"rr", b"\xff\xff"] {
        call(
            &transport,
            RawKvReq::put(Bytes::copy_from_slice(key), &b"stored"[..]),
        )
        .await
        .unwrap();
        assert_eq!(
            call(&transport, RawKvReq::get(Bytes::copy_from_slice(key)))
                .await
                .unwrap(),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"stored"))
            },
            "key {key:?} did not round trip"
        );
    }

    // And they all come back as the keys that were sent, not as stored keys.
    let RawKvResp::Scan { pairs } = call(&transport, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap()
    else {
        panic!("not a scan response");
    };
    let keys: Vec<Vec<u8>> = pairs.iter().map(|(key, _)| key.to_vec()).collect();
    assert_eq!(
        keys,
        vec![
            b"".to_vec(),
            b"r".to_vec(),
            b"rr".to_vec(),
            b"\xff\xff".to_vec()
        ],
    );

    running.handle.shutdown().await.unwrap();
}

/// `CLAUDE.md` invariant 5 over the wire: a stale epoch is refused, and the error carries the
/// region the client should adopt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_epoch_is_refused_with_a_redirect_hint() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let stale = RequestHeader::new(1, Epoch::new(1, 0), 0);
    let error = transport
        .call(Request::raw_kv(stale, RawKvReq::get(&b"k"[..])))
        .await
        .expect_err("a stale epoch was served");

    match &error {
        ProtoError::EpochNotMatch { current_regions } => {
            assert_eq!(current_regions.len(), 1);
            assert_eq!(current_regions[0].id, 1);
            assert_eq!(current_regions[0].epoch, Epoch::INITIAL);
            assert!(
                current_regions[0].end_key.is_empty(),
                "the first region covers the whole key space"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(error.is_retryable(), "a client must be able to act on this");

    // The connection is unharmed, and a correct header still works.
    assert_eq!(
        call(&transport, RawKvReq::get(&b"k"[..])).await.unwrap(),
        RawKvResp::Get { value: None }
    );

    running.handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_for_another_region_is_refused() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let elsewhere = RequestHeader::new(99, Epoch::INITIAL, 0);
    let error = transport
        .call(Request::raw_kv(elsewhere, RawKvReq::get(&b"k"[..])))
        .await
        .expect_err("a request for another region was served");
    assert_eq!(error, ProtoError::RegionNotFound { region_id: 99 });

    running.handle.shutdown().await.unwrap();
}

/// A frame larger than the server accepts never reaches it, and one that arrives anyway is
/// refused without taking the server down.
///
/// Two halves, because the fix for the first does not remove the need for the second. Since the
/// handshake reports the server's `max_frame_size`, a client narrows its own sending limit to it
/// and refuses an oversized request locally — one failed call, connection intact. A peer that did
/// not (an older build, a hostile one) still gets its frame refused by the reader, which cannot
/// answer it: a bad length means the reader no longer knows where the next frame begins, so it
/// closes the connection rather than guessing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_is_refused_and_the_server_survives() {
    let small = TransportConfig {
        max_frame_size: 64 * 1024,
        ..TransportConfig::new()
    };
    let dir = TempDir::new().unwrap();
    let running = start_at(dir.path(), small).await;
    let address = running.handle.local_addr();

    // A client whose *own* limit is the 16 MiB default, so only the handshake stops it.
    let generous = TcpTransport::connect_with(address, TransportConfig::new())
        .await
        .unwrap();
    assert_eq!(
        generous.max_frame_size(),
        64 * 1024,
        "the client did not adopt the limit the server advertised"
    );

    let huge = RawKvReq::put(&b"k"[..], Bytes::from(vec![b'v'; 256 * 1024]));
    let error = call(&generous, huge)
        .await
        .expect_err("an oversized request was sent");
    assert_eq!(
        error.outcome(),
        RequestOutcome::NotApplied,
        "a request refused before it was sent cannot have applied: {error:?}"
    );
    // Refused locally, so the connection is untouched and still usable.
    assert!(
        !generous.is_closed(),
        "a local refusal closed the connection"
    );
    assert_eq!(
        call(&generous, RawKvReq::put(&b"small"[..], &b"v"[..]))
            .await
            .unwrap(),
        RawKvResp::Put
    );

    // And a peer that ignores the advertised limit: the frame is refused, the connection goes,
    // and the server does not.
    let mut rude = TcpStream::connect(address).await.unwrap();
    let hello = Frame::new(
        FrameKind::Request,
        1,
        Bytes::from(Request::Hello(Hello::current()).encode()),
    );
    rude.write_all(&hello.encode(MAX_FRAME_SIZE).unwrap())
        .await
        .unwrap();
    let oversized = Frame::new(FrameKind::Request, 2, Bytes::from(vec![0u8; 256 * 1024]));
    let _ = rude
        .write_all(&oversized.encode(MAX_FRAME_SIZE).unwrap())
        .await;

    // The server closes on us. Whatever it had already queued drains first, so read until the
    // end of the stream; the loop ending at all is the assertion — a server that kept the
    // connection would leave this read waiting until the test's own timeout.
    let mut scratch = [0u8; 4096];
    while let Ok(read) = rude.read(&mut scratch).await {
        if read == 0 {
            break;
        }
    }

    let second = TcpTransport::connect_with(address, small).await.unwrap();
    assert_eq!(
        call(&second, RawKvReq::put(&b"after"[..], &b"ok"[..]))
            .await
            .unwrap(),
        RawKvResp::Put
    );

    running.handle.shutdown().await.unwrap();
}

/// A response too large for the frame cannot happen, because a scan is bounded by bytes as
/// well as by count. Without that bound this test would fail with a framing error rather than
/// a short answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scan_of_large_values_is_bounded_by_bytes() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    // 16 MiB of values in total: more than one frame could ever carry.
    let value = Bytes::from(vec![b'x'; 512 * 1024]);
    for index in 0..32u8 {
        call(
            &transport,
            RawKvReq::put(Bytes::from(vec![b'k', index]), value.clone()),
        )
        .await
        .unwrap();
    }

    let RawKvResp::Scan { pairs } = call(&transport, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap()
    else {
        panic!("not a scan response");
    };
    assert!(!pairs.is_empty(), "the scan returned nothing at all");
    assert!(
        pairs.len() < 32,
        "the scan returned everything; the byte budget did not apply"
    );
    let total: usize = pairs.iter().map(|(k, v)| k.len() + v.len()).sum();
    assert!(
        total < MAX_FRAME_SIZE,
        "a scan built a response no frame can carry: {total} bytes"
    );

    running.handle.shutdown().await.unwrap();
}

/// Version negotiation happens on connect and there is no downgrade path
/// (`docs/DESIGN.md` §9). Hand-rolled, because a `TcpTransport` cannot be made to lie about
/// which version it speaks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_at_the_wrong_wire_version_is_refused() {
    let running = start().await;
    let mut socket = TcpStream::connect(running.handle.local_addr())
        .await
        .unwrap();

    let hello = Request::Hello(Hello {
        version: WIRE_VERSION + 7,
    });
    let frame = Frame::new(FrameKind::Request, 1, Bytes::from(hello.encode()));
    socket
        .write_all(&frame.encode(MAX_FRAME_SIZE).unwrap())
        .await
        .unwrap();

    let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
    let mut buffer = bytes::BytesMut::new();
    let answer = loop {
        let read = socket.read_buf(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "the server closed without answering");
        decoder.push(&buffer.split());
        if let Some(frame) = decoder.next_frame().unwrap() {
            break frame;
        }
    };
    assert_eq!(answer.kind, FrameKind::Error);
    assert_eq!(
        ProtoError::decode(&answer.body).unwrap(),
        ProtoError::WireVersion {
            expected: WIRE_VERSION,
            actual: WIRE_VERSION + 7,
        }
    );

    // A client at the right version is unaffected.
    let good = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();
    assert_eq!(good.hello_ack().version, WIRE_VERSION);
    assert_eq!(good.hello_ack().store_id, 1);

    running.handle.shutdown().await.unwrap();
}

/// Many clients, many connections, overlapping writes and reads. A smoke test in the sense
/// that it is not looking for a specific bug — it is looking for the ones that only appear
/// when the store is being used by more than one caller at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_do_not_interfere() {
    const CLIENTS: u32 = 16;
    const KEYS: u32 = 25;

    let running = start().await;
    let address = running.handle.local_addr();

    let mut clients = Vec::new();
    for client in 0..CLIENTS {
        clients.push(tokio::spawn(async move {
            let transport = TcpTransport::connect(address).await?;
            for index in 0..KEYS {
                let key = Bytes::from(format!("client-{client}/key-{index}"));
                let value = Bytes::from(format!("value-{client}-{index}"));
                // Unsynced: this test is about interference, not durability, and 400 fsyncs
                // would make it a disk benchmark.
                call(
                    &transport,
                    RawKvReq::put(key.clone(), value.clone()).unsynced(),
                )
                .await?;
                assert_eq!(
                    call(&transport, RawKvReq::get(key.clone())).await?,
                    RawKvResp::Get { value: Some(value) },
                    "client {client} read back someone else's value for {key:?}"
                );
            }
            Ok::<(), ProtoError>(())
        }));
    }
    for client in clients {
        client.await.unwrap().unwrap();
    }

    // Every key from every client is there, exactly once.
    let transport = TcpTransport::connect(address).await.unwrap();
    let RawKvResp::Scan { pairs } = call(&transport, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap()
    else {
        panic!("not a scan response");
    };
    assert_eq!(u32::try_from(pairs.len()).unwrap(), CLIENTS * KEYS);

    running.handle.shutdown().await.unwrap();
}

/// A `sync = true` write that has been acknowledged is on disk. The store is dropped and the
/// directory reopened, which is `CLAUDE.md` invariant 1 seen from the far end of a socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_durable_write_survives_the_store_being_reopened() {
    // The directory outlives the server on purpose: the point of this test is what is left
    // behind on the disk after the store that wrote it is gone.
    let dir = TempDir::new().unwrap();

    {
        let running = start_at(dir.path(), TransportConfig::new()).await;
        let transport = TcpTransport::connect(running.handle.local_addr())
            .await
            .unwrap();
        // The default is durable: `sync = true` is what a caller gets without asking.
        assert!(RawKvReq::put(&b"k"[..], &b"v"[..]).is_sync());
        call(&transport, RawKvReq::put(&b"durable"[..], &b"yes"[..]))
            .await
            .unwrap();

        running.handle.shutdown().await.unwrap();
        drop(running.store);
    }

    let reopened = Store::open(dir.path(), StoreOptions::new()).unwrap();
    assert_eq!(
        reopened
            .handle(header(), RawKvReq::get(&b"durable"[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"yes"))
        }
    );
}

/// Shutdown is graceful: the requests already running finish and are answered, so the caller
/// can close the database knowing no handler is still writing to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_answers_the_requests_already_running() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let mut writes = Vec::new();
    for index in 0..64u32 {
        let transport = transport.clone();
        writes.push(tokio::spawn(async move {
            call(
                &transport,
                RawKvReq::put(Bytes::from(format!("k-{index}")), &b"v"[..]).unsynced(),
            )
            .await
        }));
    }

    // Stop while those are in flight — but only once at least one of them has demonstrably
    // reached the store.
    //
    // **`written > 0` below is a vacuity guard, not the property.** The property is the comment
    // beside it: an answer must never say a write succeeded when the store was already closed.
    // That, and `pairs.len() >= written`, both hold when nothing was ever in flight — so the
    // guard is what stops the test passing while testing nothing, and a fixed sleep only *bets*
    // that the guard will be true. Under a saturated `--workspace` run the bet loses: sixty-four
    // freshly spawned tasks need not have been scheduled at all inside a millisecond, the
    // shutdown then refuses all sixty-four, and the failure reads "the shutdown answered nothing
    // at all" when what actually happened is that the test did not run
    // (`docs/plans/debt-c1.md` section 6).
    //
    // Waiting on the store's own state instead makes non-vacuity a fact. The write that landed
    // is answered, so the guard is true by construction; the rest are still in flight, which is
    // what the test is for. A longer sleep would only have moved the bet.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let RawKvResp::Scan { pairs } = running
            .store
            .handle(header(), RawKvReq::scan(&b""[..], &b""[..], 0))
            .unwrap()
        else {
            panic!("not a scan response");
        };
        if !pairs.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not one of the writes reached the store, so there was nothing in flight to shut \
             down and this test could not have tested anything"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    running.handle.shutdown().await.unwrap();

    let mut written = 0;
    for write in writes {
        // A request was either answered or never accepted. What must not happen is an answer
        // saying the write succeeded when the store had already been closed underneath it.
        if write.await.unwrap().is_ok() {
            written += 1;
        }
    }
    assert!(written > 0, "the shutdown answered nothing at all");

    // Everything that was acknowledged is readable, which is the property that would break if
    // the shutdown closed the database out from under a running handler.
    let RawKvResp::Scan { pairs } = running
        .store
        .handle(header(), RawKvReq::scan(&b""[..], &b""[..], 0))
        .unwrap()
    else {
        panic!("not a scan response");
    };
    assert!(
        pairs.len() >= written,
        "{written} writes were acknowledged but only {} are stored",
        pairs.len()
    );
}
