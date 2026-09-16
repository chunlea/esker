//! **What a request says it encodes to is what it encodes to** — debt #98.
//!
//! A client refuses an oversized request before sending it, and until this existed it refused by an
//! estimate: six bytes a field, and — since [ADR 0114](../../../docs/adr/0114-a-unique-key-being-written-waits-at-read-committed.md)
//! §2 gave mutations a read timestamp — six more for a varint. An estimate that runs high refuses
//! requests that would have gone out, which is what run 127 attempt 7 met on a 65,536-row fixture
//! load: "about 17334584 bytes" against a 16,777,216-byte limit, for a frame of about 15.8 MB.
//!
//! So `encoded_len` is not an estimate and this file is why: every variant of both request bodies is
//! encoded and measured, and the two numbers have to agree exactly. A field added to an encoder
//! without its length here fails this test rather than drifting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_proto::messages::{RawKvReq, Request, RequestHeader};
use esker_proto::txn::{TxnKvReq, TxnMutation};
use esker_proto::{Epoch, MAX_REQUEST_ENVELOPE};

/// The method is two bytes in front of the header, which is the only part of a request frame that
/// is neither the body nor the frame's own header.
const METHOD: usize = 2;

fn key(at: u8) -> Bytes {
    Bytes::from(vec![b'k', at])
}

/// A header with room-sized numbers, so the body's length is what the arithmetic isolates.
fn header() -> RequestHeader {
    RequestHeader::new(7, Epoch::new(3, 9), 2)
}

/// A timestamp wide enough that its varint is nine bytes, which is what a real TSO hands out and
/// the width the estimate used to get wrong.
const TS: u64 = 1_757_000_000_000_u64 << 18;

fn txn_corpus() -> Vec<TxnKvReq> {
    let mutations = vec![
        TxnMutation::Put {
            key: key(1),
            value: Bytes::from_static(b"a value"),
            read_ts: None,
        },
        TxnMutation::Put {
            key: key(2),
            value: Bytes::from_static(b"a value"),
            read_ts: Some(TS),
        },
        TxnMutation::Delete {
            key: key(3),
            read_ts: None,
        },
        TxnMutation::Delete {
            key: key(4),
            read_ts: Some(TS),
        },
        TxnMutation::Check {
            key: key(5),
            read_ts: None,
        },
        TxnMutation::Check {
            key: key(6),
            read_ts: Some(TS),
        },
        TxnMutation::CheckRange {
            start: key(7),
            end: key(8),
        },
    ];
    let keys = vec![key(1), key(2), key(3)];
    vec![
        TxnKvReq::Get {
            key: key(1),
            ts: TS,
        },
        TxnKvReq::Scan {
            start: key(1),
            end: key(9),
            limit: 4_096,
            ts: TS,
            reverse: true,
        },
        TxnKvReq::Prewrite {
            start_ts: TS,
            primary: key(1),
            ttl_ms: 3_000,
            mutations,
        },
        // A prewrite of nothing still carries its count.
        TxnKvReq::Prewrite {
            start_ts: TS,
            primary: Bytes::new(),
            ttl_ms: 0,
            mutations: Vec::new(),
        },
        TxnKvReq::Commit {
            start_ts: TS,
            commit_ts: TS + 1,
            keys: keys.clone(),
        },
        TxnKvReq::Rollback {
            start_ts: TS,
            keys: keys.clone(),
        },
        TxnKvReq::ReleaseLock {
            start_ts: TS,
            keys: keys.clone(),
        },
        TxnKvReq::ResolveLock {
            start_ts: TS,
            commit_ts: 0,
            keys,
        },
        TxnKvReq::Heartbeat {
            start_ts: TS,
            primary: key(1),
            ttl_ms: 3_000,
        },
        TxnKvReq::GcSafepoint { safepoint: TS },
        TxnKvReq::ReclaimRange {
            start: key(1),
            end: key(9),
            below_ts: TS,
        },
        TxnKvReq::LatestCommit { key: key(1) },
    ]
}

fn raw_corpus() -> Vec<RawKvReq> {
    vec![
        RawKvReq::Get { key: key(1) },
        RawKvReq::BatchGet {
            keys: vec![key(1), key(2)],
        },
        RawKvReq::BatchGet { keys: Vec::new() },
        RawKvReq::Put {
            key: key(1),
            value: Bytes::from(vec![0u8; 300]),
            sync: true,
        },
        RawKvReq::BatchPut {
            pairs: vec![(key(1), Bytes::from_static(b"one")), (key(2), Bytes::new())],
            sync: false,
        },
        RawKvReq::Delete {
            key: key(1),
            sync: true,
        },
        RawKvReq::DeleteRange {
            start: key(1),
            end: key(9),
            sync: false,
        },
        RawKvReq::Scan {
            start: key(1),
            end: key(9),
            limit: 128,
            reverse: true,
        },
        RawKvReq::CompareAndSwap {
            key: key(1),
            expected: None,
            value: Some(Bytes::from_static(b"v")),
            sync: true,
        },
        RawKvReq::CompareAndSwap {
            key: key(1),
            expected: Some(Bytes::from_static(b"old")),
            value: None,
            sync: false,
        },
    ]
}

/// The body of a request frame is `method ++ header ++ fields`, so what the fields cost is what is
/// left when the first two are taken off.
fn body_of(request: &Request) -> usize {
    request.encode().len() - METHOD - header().encoded_len()
}

#[test]
fn every_txn_request_encodes_to_the_length_it_reports() {
    for request in txn_corpus() {
        let reported = request.encoded_len();
        let encoded = body_of(&Request::txn_kv(header(), request.clone()));
        assert_eq!(
            reported,
            encoded,
            "{:?} reports {reported} bytes and encodes to {encoded}",
            request.method()
        );
    }
}

#[test]
fn every_raw_request_encodes_to_the_length_it_reports() {
    for request in raw_corpus() {
        let reported = request.encoded_len();
        let encoded = body_of(&Request::raw_kv(header(), request.clone()));
        assert_eq!(
            reported,
            encoded,
            "{:?} reports {reported} bytes and encodes to {encoded}",
            request.method()
        );
    }
}

/// The envelope a client adds to a body when it asks whether the frame will fit: the frame header,
/// the method, and the header — whose widest form is four `u64` varints.
#[test]
fn no_request_envelope_is_wider_than_the_allowance_a_client_reserves() {
    let widest = RequestHeader::new(u64::MAX, Epoch::new(u64::MAX, u64::MAX), u64::MAX);
    let request = TxnKvReq::LatestCommit { key: key(1) };
    let frame =
        esker_proto::FRAME_HEADER_SIZE + Request::txn_kv(widest, request.clone()).encode().len();
    assert!(
        frame <= request.encoded_len() + MAX_REQUEST_ENVELOPE,
        "a frame of {frame} bytes over a body of {} needs more than {MAX_REQUEST_ENVELOPE}",
        request.encoded_len()
    );
}
