//! Message bodies, pinned byte for byte and fuzzed.
//!
//! The golden file was produced by an encoder written separately from the one under test, so
//! it checks the implementation rather than agreeing with it. Forty message bodies is a lot to
//! pin, and that is the point: a wire format drifts one field at a time, and the field nobody
//! wrote a golden for is the one that moves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_proto::messages::{DEFAULT_SCAN_LIMIT, Hello, HelloAck, RawKvReq, RawKvResp};
use esker_proto::{
    Epoch, MAX_FRAME_SIZE, Method, Peer, PeerRole, ProtoError, Region, Request, RequestHeader,
    Response, WIRE_VERSION,
};
use proptest::prelude::*;

const GOLDEN: &str = include_str!("golden/messages.hex");

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn golden(kind: &str, name: &str) -> Vec<u8> {
    let prefix = format!("{kind} {name} ");
    let line = GOLDEN
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in the golden file"));
    let text = line.trim();
    assert!(text.len() % 2 == 0, "odd-length hex for `{prefix}`");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("bad hex"))
        .collect()
}

/// The header every golden request carries: region 1, epoch (2, 3), peer 4.
fn header() -> RequestHeader {
    RequestHeader::new(1, Epoch::new(2, 3), 4)
}

fn golden_requests() -> Vec<(&'static str, Request)> {
    let h = header();
    vec![
        ("hello", Request::Hello(Hello::current())),
        ("get", Request::raw_kv(h, RawKvReq::get(&b"key"[..]))),
        (
            "batch-get",
            Request::raw_kv(
                h,
                RawKvReq::BatchGet {
                    keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
                },
            ),
        ),
        (
            "put",
            Request::raw_kv(h, RawKvReq::put(&b"k"[..], &b"v"[..])),
        ),
        (
            "put-unsynced",
            Request::raw_kv(h, RawKvReq::put(&b"k"[..], &b"v"[..]).unsynced()),
        ),
        (
            "batch-put",
            Request::raw_kv(
                h,
                RawKvReq::batch_put(vec![
                    (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                    (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
                ]),
            ),
        ),
        ("delete", Request::raw_kv(h, RawKvReq::delete(&b"k"[..]))),
        (
            "delete-range",
            Request::raw_kv(h, RawKvReq::delete_range(&b"a"[..], &b"m"[..])),
        ),
        (
            "scan",
            Request::raw_kv(h, RawKvReq::scan(&b"a"[..], &b"z"[..], 100)),
        ),
        (
            "scan-reverse",
            Request::raw_kv(
                h,
                RawKvReq::Scan {
                    start: Bytes::from_static(b"z"),
                    end: Bytes::new(),
                    limit: 0,
                    reverse: true,
                },
            ),
        ),
        (
            "cas",
            Request::raw_kv(
                h,
                RawKvReq::compare_and_swap(
                    &b"k"[..],
                    Some(Bytes::from_static(b"old")),
                    Some(Bytes::from_static(b"new")),
                ),
            ),
        ),
        (
            "cas-absent",
            Request::raw_kv(h, RawKvReq::compare_and_swap(&b"k"[..], None, None)),
        ),
    ]
}

fn golden_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "hello",
            Response::Hello(HelloAck {
                version: WIRE_VERSION,
                store_id: 7,
                max_frame_size: MAX_FRAME_SIZE as u64,
            }),
        ),
        (
            "get",
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::from_static(b"v")),
            }),
        ),
        (
            "get-absent",
            Response::RawKv(RawKvResp::Get { value: None }),
        ),
        (
            "get-empty",
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::new()),
            }),
        ),
        (
            "batch-get",
            Response::RawKv(RawKvResp::BatchGet {
                values: vec![Some(Bytes::from_static(b"a")), None],
            }),
        ),
        ("put", Response::RawKv(RawKvResp::Put)),
        ("batch-put", Response::RawKv(RawKvResp::BatchPut)),
        ("delete", Response::RawKv(RawKvResp::Delete)),
        (
            "delete-range",
            Response::RawKv(RawKvResp::DeleteRange { deleted: 17 }),
        ),
        (
            "scan",
            Response::RawKv(RawKvResp::Scan {
                pairs: vec![
                    (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                    (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
                ],
            }),
        ),
        (
            "cas-failed",
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: false,
                previous: Some(Bytes::from_static(b"actual")),
            }),
        ),
        (
            "cas-swapped",
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: true,
                previous: None,
            }),
        ),
    ]
}

// A list of sixteen literals, one per error code. Splitting it to satisfy a line count would
// make it harder to check against the golden file, which is the whole job of this function.
#[allow(clippy::too_many_lines)]
fn golden_errors() -> Vec<(&'static str, ProtoError)> {
    vec![
        (
            "not-leader",
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(7),
            },
        ),
        (
            "not-leader-blind",
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            },
        ),
        (
            "epoch-not-match",
            ProtoError::EpochNotMatch {
                current_regions: vec![
                    Region {
                        id: 1,
                        start_key: Bytes::new(),
                        end_key: Bytes::from_static(b"m"),
                        peers: vec![Peer::voter(1, 2)],
                        epoch: Epoch::new(1, 2),
                    },
                    Region {
                        id: 2,
                        start_key: Bytes::from_static(b"m"),
                        end_key: Bytes::new(),
                        peers: vec![Peer {
                            store_id: 1,
                            peer_id: 3,
                            role: PeerRole::Learner,
                        }],
                        epoch: Epoch::new(1, 2),
                    },
                ],
            },
        ),
        (
            "key-not-in-region",
            ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b"z"),
                region_id: 3,
                start_key: Bytes::from_static(b"a"),
                end_key: Bytes::from_static(b"m"),
            },
        ),
        (
            "server-is-busy",
            ProtoError::ServerIsBusy {
                reason: "write stall".to_owned(),
            },
        ),
        (
            "locked",
            ProtoError::Locked {
                lock_info: Bytes::from_static(&[1, 2, 3]),
            },
        ),
        (
            "region-not-found",
            ProtoError::RegionNotFound { region_id: 9 },
        ),
        (
            "wire-version",
            ProtoError::WireVersion {
                expected: 1,
                actual: 2,
            },
        ),
        (
            "invalid-request",
            ProtoError::InvalidRequest {
                detail: "unknown method 0x0999".to_owned(),
            },
        ),
        (
            "unsupported",
            ProtoError::Unsupported {
                detail: "DeleteRange over 10000 keys".to_owned(),
            },
        ),
        (
            "corrupt",
            ProtoError::Corrupt {
                context: "frame".to_owned(),
                detail: "checksum mismatch".to_owned(),
            },
        ),
        (
            "io",
            ProtoError::Io {
                detail: "broken pipe".to_owned(),
            },
        ),
        (
            "closed",
            ProtoError::Closed {
                detail: "peer went away".to_owned(),
            },
        ),
        (
            "duplicate-id",
            ProtoError::DuplicateRequestId { request_id: 42 },
        ),
        (
            "internal",
            ProtoError::Internal {
                detail: "poisoned lock".to_owned(),
            },
        ),
        ("not-sent", ProtoError::not_sent("connection refused")),
    ]
}

#[test]
fn golden_request_bodies() {
    for (name, request) in golden_requests() {
        assert_eq!(
            hex(&request.encode()),
            hex(&golden("request", name)),
            "request `{name}` drifted"
        );
        assert_eq!(Request::decode(&golden("request", name)).unwrap(), request);
    }
}

#[test]
fn golden_response_bodies() {
    for (name, response) in golden_responses() {
        assert_eq!(
            hex(&response.encode()),
            hex(&golden("response", name)),
            "response `{name}` drifted"
        );
        assert_eq!(
            Response::decode(&golden("response", name)).unwrap(),
            response
        );
    }
}

#[test]
fn golden_error_bodies() {
    for (name, error) in golden_errors() {
        assert_eq!(
            hex(&error.encode()),
            hex(&golden("error", name)),
            "error `{name}` drifted"
        );
        assert_eq!(ProtoError::decode(&golden("error", name)).unwrap(), error);
    }
}

/// Every method and every error code has a golden. A format is pinned by the case nobody
/// wrote, so the sweep is what makes the file above trustworthy.
#[test]
fn the_goldens_cover_every_method_and_every_error_code() {
    let pinned_requests: std::collections::BTreeSet<Method> = golden_requests()
        .iter()
        .map(|(_, request)| request.method())
        .collect();
    let pinned_responses: std::collections::BTreeSet<Method> = golden_responses()
        .iter()
        .map(|(_, response)| response.method())
        .collect();
    let all: std::collections::BTreeSet<Method> = Method::ALL.into_iter().collect();
    assert_eq!(pinned_requests, all, "a method has no golden request");
    assert_eq!(pinned_responses, all, "a method has no golden response");

    let pinned_codes: std::collections::BTreeSet<u16> = golden_errors()
        .iter()
        .map(|(_, error)| error.code())
        .collect();
    let all_codes: std::collections::BTreeSet<u16> =
        esker_proto::error::code::ALL.into_iter().collect();
    assert_eq!(pinned_codes, all_codes, "an error code has no golden");
}

proptest! {
    /// Anything this build can build, it can read back — including the shapes a hand-written
    /// golden would not think of: empty keys, huge values, absurd limits.
    #[test]
    fn any_raw_kv_request_round_trips(
        region_id: u64,
        conf_ver: u64,
        version: u64,
        peer: u64,
        key in proptest::collection::vec(any::<u8>(), 0..64),
        value in proptest::collection::vec(any::<u8>(), 0..512),
        limit: u32,
        reverse: bool,
        sync: bool,
        which in 0usize..8,
    ) {
        let header = RequestHeader::new(region_id, Epoch::new(conf_ver, version), peer);
        let key = Bytes::from(key);
        let value = Bytes::from(value);
        let request = match which {
            0 => RawKvReq::Get { key },
            1 => RawKvReq::BatchGet { keys: vec![key, value] },
            2 => RawKvReq::Put { key, value, sync },
            3 => RawKvReq::BatchPut { pairs: vec![(key, value)], sync },
            4 => RawKvReq::Delete { key, sync },
            5 => RawKvReq::DeleteRange { start: key, end: value, sync },
            6 => RawKvReq::Scan { start: key, end: value, limit, reverse },
            _ => RawKvReq::CompareAndSwap {
                key,
                expected: reverse.then_some(value.clone()),
                value: sync.then_some(value),
                sync,
            },
        };
        let message = Request::raw_kv(header, request);
        prop_assert_eq!(Request::decode(&message.encode()).unwrap(), message);
    }

    /// The fuzz case for the body decoder: whatever bytes a frame carried, decoding is a
    /// value or an error, never a panic (`CLAUDE.md` invariant 9).
    #[test]
    fn random_bodies_never_panic(body in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = Request::decode(&body);
        let _ = Response::decode(&body);
        let _ = ProtoError::decode(&body);
    }

    /// A plausible body is the harder case: a real message with a few bytes changed is where
    /// a decoder is most tempted to trust a length or a count.
    #[test]
    fn damaged_bodies_never_panic(
        which in 0usize..12,
        damage in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
    ) {
        let (_, request) = golden_requests().swap_remove(which);
        let mut bytes = request.encode();
        for (at, value) in damage {
            let at = at % bytes.len();
            bytes[at] = value;
        }
        let _ = Request::decode(&bytes);
        let _ = Response::decode(&bytes);
        let _ = ProtoError::decode(&bytes);
    }
}

/// A scan with no limit is still a bounded answer, or one region's worth of keys would have to
/// fit in one frame.
#[test]
fn an_unlimited_scan_still_has_a_default_bound() {
    assert!(DEFAULT_SCAN_LIMIT > 0);
    let request = RawKvReq::scan(&b""[..], &b""[..], 0);
    match request {
        RawKvReq::Scan { limit, .. } => assert_eq!(limit, 0, "zero on the wire means the default"),
        other => panic!("{other:?}"),
    }
}
