//! The frame layout, pinned and fuzzed.
//!
//! The golden file was produced by an encoder written separately from the one under test — a
//! bit-at-a-time CRC32C and a hand-written layout — so it checks the implementation rather
//! than agreeing with it. Everything else here is `CLAUDE.md` invariant 9 at the network
//! edge: these bytes come off a socket that anyone can write to, so no arrangement of them
//! may panic.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::{Bytes, BytesMut};
use esker_proto::frame::{FrameDecoder, MIN_LEN_FIELD, frame_checksum};
use esker_proto::{FRAME_HEADER_SIZE, Frame, FrameKind, MAX_FRAME_SIZE, ProtoError};
use proptest::prelude::*;

const GOLDEN: &str = include_str!("golden/frames.hex");

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "odd-length hex");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("bad hex"))
        .collect()
}

/// A field of the golden file: `frame <name> <hex>` or `stream <hex>`.
fn golden(kind: &str, name: &str) -> Vec<u8> {
    let prefix = if name.is_empty() {
        format!("{kind} ")
    } else {
        format!("{kind} {name} ")
    };
    let line = GOLDEN
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in the golden file"));
    unhex(line.trim())
}

/// CRC32C computed one bit at a time, independent of the table-driven and hardware
/// implementations in `esker-base`. A golden file checked by the code that wrote it proves
/// only that the code is consistent with itself.
fn crc32c_bitwise(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn the_independent_checksum_matches_its_published_check_value() {
    assert_eq!(crc32c_bitwise(b"123456789"), 0xE306_9283);
}

fn golden_cases() -> Vec<(&'static str, Frame)> {
    vec![
        ("ping-empty", Frame::empty(FrameKind::Ping, 0)),
        (
            "request-get",
            Frame::new(FrameKind::Request, 1, Bytes::from_static(&[0x01, 0x01])),
        ),
        (
            "response-max-id",
            Frame::new(
                FrameKind::Response,
                u64::MAX,
                Bytes::from_static(&[0, 1, 2, 3, 4, 5, 6, 7]),
            ),
        ),
        (
            "stream-chunk",
            Frame::new(FrameKind::Stream, 42, Bytes::from(vec![0xAA; 16])),
        ),
        ("stream-end", Frame::empty(FrameKind::StreamEnd, 42)),
        (
            "error-busy",
            Frame::new(
                FrameKind::Error,
                7,
                Bytes::from(
                    ProtoError::ServerIsBusy {
                        reason: "write stall".to_owned(),
                    }
                    .encode(),
                ),
            ),
        ),
    ]
}

/// The frames of `docs/DESIGN.md` §9, byte for byte.
#[test]
fn golden_frames() {
    for (name, frame) in golden_cases() {
        let expected = golden("frame", name);
        let encoded = frame.encode(MAX_FRAME_SIZE).unwrap();
        assert_eq!(hex(&encoded), hex(&expected), "frame `{name}` drifted");

        // And the golden bytes decode back to what they were built from.
        assert_eq!(Frame::decode(&expected, MAX_FRAME_SIZE).unwrap(), frame);
    }
}

/// Every golden frame's checksum, recomputed by an implementation that shares no code with
/// the one under test, over exactly `kind ++ request_id ++ body`.
#[test]
fn golden_checksums_come_out_of_an_independent_implementation() {
    for (name, frame) in golden_cases() {
        let bytes = golden("frame", name);
        let stored = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(stored, crc32c_bitwise(&bytes[8..]), "`{name}` checksum");
        assert_eq!(
            stored,
            frame_checksum(frame.kind, frame.request_id, &frame.body),
            "`{name}` checksum helper"
        );
    }
}

/// The `len` field counts the bytes after itself, and nothing else. One off-by-four here and
/// every peer loses the stream after the first frame.
#[test]
fn golden_lengths_count_the_bytes_after_the_length_field() {
    for (name, _) in golden_cases() {
        let bytes = golden("frame", name);
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4, "`{name}`");
        assert!(len >= MIN_LEN_FIELD, "`{name}`");
    }
}

/// What a reader actually sees: frames back to back with no separator.
#[test]
fn the_golden_stream_decodes_to_every_frame_in_order() {
    let stream = golden("stream", "");
    let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
    decoder.push(&stream);

    for (name, expected) in golden_cases() {
        let frame = decoder
            .next_frame()
            .unwrap_or_else(|error| panic!("`{name}`: {error}"))
            .unwrap_or_else(|| panic!("`{name}` was not in the stream"));
        assert_eq!(frame, expected, "`{name}`");
    }
    assert_eq!(decoder.next_frame().unwrap(), None);
    assert_eq!(decoder.buffered(), 0);
}

proptest! {
    /// TCP delivers arbitrary chunks, so the reader has to be indifferent to where the splits
    /// fall. The golden stream is fed in randomly sized pieces and must produce exactly the
    /// same six frames.
    #[test]
    fn the_golden_stream_survives_any_chunking(splits in proptest::collection::vec(1usize..40, 0..64)) {
        let stream = golden("stream", "");
        let expected: Vec<Frame> = golden_cases().into_iter().map(|(_, frame)| frame).collect();

        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        let mut decoded = Vec::new();
        let mut at = 0;
        let mut sizes = splits.into_iter().cycle();
        while at < stream.len() {
            let take = sizes.next().unwrap_or(1).min(stream.len() - at);
            decoder.push(&stream[at..at + take]);
            at += take;
            while let Some(frame) = decoder.next_frame().unwrap() {
                decoded.push(frame);
            }
        }
        prop_assert_eq!(decoded, expected);
        prop_assert_eq!(decoder.buffered(), 0);
    }

    /// Any frame this build can build, it can read back.
    #[test]
    fn any_frame_round_trips(
        kind in 0usize..FrameKind::ALL.len(),
        request_id: u64,
        body in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let frame = Frame::new(FrameKind::ALL[kind], request_id, Bytes::from(body));
        let encoded = frame.encode(MAX_FRAME_SIZE).unwrap();
        prop_assert_eq!(encoded.len(), frame.encoded_len());
        prop_assert_eq!(Frame::decode(&encoded, MAX_FRAME_SIZE).unwrap(), frame);
    }

    /// A batch of frames written back to back comes out one at a time, in order.
    #[test]
    fn a_batch_of_frames_keeps_its_order(
        bodies in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 1..16),
    ) {
        let frames: Vec<Frame> = bodies
            .into_iter()
            .enumerate()
            .map(|(index, body)| Frame::new(FrameKind::Request, index as u64, Bytes::from(body)))
            .collect();
        let mut stream = BytesMut::new();
        for frame in &frames {
            frame.encode_into(&mut stream, MAX_FRAME_SIZE).unwrap();
        }

        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&stream);
        let mut decoded = Vec::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            decoded.push(frame);
        }
        prop_assert_eq!(decoded, frames);
    }

    /// The fuzz test the phase prompt asks for: whatever bytes arrive, the decoder returns a
    /// frame or an error. It never panics, never hangs, and never allocates on a length it
    /// has not checked (`CLAUDE.md` invariant 9).
    #[test]
    fn random_bytes_never_panic(noise in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&noise);
        // Stops on `Ok(None)` — needs more bytes — and on any error. Either is fine; a panic
        // is not.
        while let Ok(Some(_frame)) = decoder.next_frame() {}
    }

    /// The same, but with bytes that start out looking like a frame — a plausible header is
    /// where a decoder is most likely to trust a length it should not.
    #[test]
    fn damaged_frames_never_panic(
        body in proptest::collection::vec(any::<u8>(), 0..256),
        damage in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
    ) {
        let frame = Frame::new(FrameKind::Request, 9, Bytes::from(body));
        let mut bytes = frame.encode(MAX_FRAME_SIZE).unwrap().to_vec();
        for (at, value) in damage {
            let at = at % bytes.len();
            bytes[at] = value;
        }
        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&bytes);
        let _ = decoder.next_frame();
    }

    /// A frame the peer could never accept must not be written either: the size limit is
    /// enforced on the way out as well as on the way in.
    #[test]
    fn a_body_over_the_limit_is_refused_at_both_ends(extra in 1usize..64) {
        let limit = FRAME_HEADER_SIZE + 32;
        let frame = Frame::new(FrameKind::Stream, 1, Bytes::from(vec![0; 32 + extra]));
        prop_assert!(frame.encode(limit).is_err());

        // Written by a peer with a larger limit, refused by a reader with a smaller one.
        let encoded = frame.encode(MAX_FRAME_SIZE).unwrap();
        let mut decoder = FrameDecoder::new(limit);
        decoder.push(&encoded);
        prop_assert!(decoder.next_frame().is_err());
    }
}

/// Corruption is reported as a value, not a panic, and it says which frame and why
/// (`CLAUDE.md` invariant 2).
#[test]
fn corruption_is_reported_with_its_context() {
    let frame = Frame::new(FrameKind::Response, 3, Bytes::from_static(b"value"));
    let mut bytes = frame.encode(MAX_FRAME_SIZE).unwrap().to_vec();
    bytes[FRAME_HEADER_SIZE] ^= 0xFF;

    match Frame::decode(&bytes, MAX_FRAME_SIZE) {
        Err(ProtoError::Corrupt { context, detail }) => {
            assert_eq!(context, "frame");
            assert!(detail.contains("checksum mismatch"), "{detail}");
        }
        other => panic!("expected a corruption error, got {other:?}"),
    }
}

/// A run of zero bytes is the shape a truncated write leaves behind. It must not be readable
/// as a frame — which is why kind 0 does not exist (`docs/DESIGN.md` §4.3 makes the same rule
/// for the WAL header).
#[test]
fn a_run_of_zeros_is_not_a_frame() {
    for len in [4usize, 17, 64, 4096] {
        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&vec![0u8; len]);
        assert!(decoder.next_frame().is_err(), "{len} zero bytes decoded");
    }
}
