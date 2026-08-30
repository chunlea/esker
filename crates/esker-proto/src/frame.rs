//! The framing (*fixed*, version 1) — `docs/DESIGN.md` §9.
//!
//! ```text
//! frame = len:u32 LE ++ crc32c:u32 LE ++ kind:u8 ++ request_id:u64 LE ++ body
//! len   = the bytes after the len field itself: 13 + body.len()
//! crc   = crc32c(kind ++ request_id LE ++ body)
//! ```
//!
//! Four properties this module owes everything above it:
//!
//! * **The checksum covers the kind and the request id, not only the body.** A flipped
//!   `request_id` whose body still checksummed would deliver a response to the wrong caller,
//!   and nothing above the framing could ever notice. It is pinned by a golden file and by a
//!   test that flips exactly that byte.
//! * **Nothing is parsed before the checksum passes.** Not the kind, not the id, not one byte
//!   of the body (`CLAUDE.md` invariant 2).
//! * **A length is checked before it is trusted.** `max_frame_size` is enforced on the way out
//!   *and* on the way in, before anything is allocated, so a corrupt or hostile length costs
//!   an error rather than a machine.
//! * **A frame may arrive in any number of pieces.** TCP has no message boundaries.
//!   [`FrameDecoder`] is a state machine over a buffer with no I/O in it, so the same code
//!   runs under a socket and under a proptest that feeds it one byte at a time.

use bytes::{Bytes, BytesMut};
use esker_base::crc32c;

use crate::ProtoError;

/// Bytes of the `len` field itself, which the field does not count.
pub const LEN_SIZE: usize = 4;

/// `len:u32 ++ crc32c:u32 ++ kind:u8 ++ request_id:u64` (`docs/DESIGN.md` §9).
pub const FRAME_HEADER_SIZE: usize = LEN_SIZE + 4 + 1 + 8;

/// Smallest legal value of the `len` field: a frame with an empty body still carries its
/// checksum, kind and request id.
pub const MIN_LEN_FIELD: usize = FRAME_HEADER_SIZE - LEN_SIZE;

/// Largest frame that will be read or written, counting the `len` field (*default*).
///
/// A larger declared length is rejected before anything is allocated for it. It has to hold a
/// snapshot chunk plus its framing (`docs/DESIGN.md` §6 streams them in 1 MiB pieces).
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Largest body that fits in a [`MAX_FRAME_SIZE`] frame.
pub const MAX_BODY_SIZE: usize = MAX_FRAME_SIZE - FRAME_HEADER_SIZE;

/// What a frame is for (*fixed*). One byte on the wire.
///
/// The numbering is **1-based, and zero is reserved**: a run of zero bytes — the shape a
/// truncated write or a sparse file leaves behind — must not read as a valid frame. It is the
/// same rule `docs/DESIGN.md` §4.3 states for the WAL record header, where "`0` is reserved
/// and never valid, so an all-zero header is not an empty record".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FrameKind {
    /// A request. The body starts with a `method:u16`.
    Request = 1,
    /// The response to the request with the same id. The body echoes the method.
    Response = 2,
    /// One chunk of a streamed response, such as a snapshot. The body *is* the chunk.
    Stream = 3,
    /// The last frame of a stream. Its body is empty.
    StreamEnd = 4,
    /// A typed error, ending the request with the same id. The body starts with a
    /// `code:u16` ([`crate::error::code`]).
    Error = 5,
    /// Liveness probe. Its body is empty and its request id is not a request id.
    Ping = 6,
    /// The reply to a [`FrameKind::Ping`], echoing its id.
    Pong = 7,
}

impl FrameKind {
    /// Every kind this version defines.
    pub const ALL: [Self; 7] = [
        Self::Request,
        Self::Response,
        Self::Stream,
        Self::StreamEnd,
        Self::Error,
        Self::Ping,
        Self::Pong,
    ];

    /// The wire byte.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The kind for a wire byte, or `None` for one this version does not define. An unknown
    /// kind is never skipped: the connection carrying it is no longer understood.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Request),
            2 => Some(Self::Response),
            3 => Some(Self::Stream),
            4 => Some(Self::StreamEnd),
            5 => Some(Self::Error),
            6 => Some(Self::Ping),
            7 => Some(Self::Pong),
            _ => None,
        }
    }

    /// Whether this kind ends the exchange its request id names.
    ///
    /// A [`FrameKind::Stream`] chunk does not; a [`FrameKind::StreamEnd`], a
    /// [`FrameKind::Response`] and an [`FrameKind::Error`] do. The demultiplexer uses this to
    /// decide when to forget a request id.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Response | Self::StreamEnd | Self::Error)
    }
}

/// One frame: a kind, the request id it belongs to, and an uninterpreted body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// What the frame is for.
    pub kind: FrameKind,
    /// The exchange this frame belongs to. Assigned by the client, unique while in flight on
    /// one connection.
    pub request_id: u64,
    /// The bytes after the header. Interpreted by [`crate::messages`], never here.
    pub body: Bytes,
}

impl Frame {
    /// A frame with the given parts.
    #[must_use]
    pub fn new(kind: FrameKind, request_id: u64, body: Bytes) -> Self {
        Self {
            kind,
            request_id,
            body,
        }
    }

    /// A frame with an empty body: a ping, a pong, the end of a stream.
    #[must_use]
    pub fn empty(kind: FrameKind, request_id: u64) -> Self {
        Self::new(kind, request_id, Bytes::new())
    }

    /// How many bytes this frame occupies on the wire, `len` field included.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        FRAME_HEADER_SIZE + self.body.len()
    }

    /// Appends the encoded frame to `out`.
    ///
    /// The size limit is enforced here as well as on the read side, so that a store cannot
    /// send a frame its peer is obliged to refuse — a failure that would otherwise appear as
    /// an unexplained disconnect at the far end.
    pub fn encode_into(&self, out: &mut BytesMut, max_frame_size: usize) -> Result<(), ProtoError> {
        let total = self.encoded_len();
        if total > max_frame_size {
            return Err(ProtoError::invalid(format!(
                "frame of {total} bytes exceeds the {max_frame_size}-byte limit"
            )));
        }

        // The limit is a `usize`, so a caller could in principle configure one that does not
        // fit the four-byte length field. Refused rather than truncated: a wrapped length
        // describes a different frame.
        let Ok(len) = u32::try_from(total - LEN_SIZE) else {
            return Err(ProtoError::invalid(format!(
                "frame of {total} bytes does not fit a 32-bit length field"
            )));
        };
        let checksum = frame_checksum(self.kind, self.request_id, &self.body);

        out.reserve(total);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&[self.kind.as_u8()]);
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(&self.body);
        Ok(())
    }

    /// The encoded frame.
    pub fn encode(&self, max_frame_size: usize) -> Result<Bytes, ProtoError> {
        let mut out = BytesMut::with_capacity(self.encoded_len());
        self.encode_into(&mut out, max_frame_size)?;
        Ok(out.freeze())
    }

    /// Decodes exactly one frame from `bytes`, which must hold nothing else.
    ///
    /// For a stream of frames use [`FrameDecoder`]; this is for the tests and for callers that
    /// already have one frame in hand.
    pub fn decode(bytes: &[u8], max_frame_size: usize) -> Result<Self, ProtoError> {
        let mut decoder = FrameDecoder::new(max_frame_size);
        decoder.push(bytes);
        let Some(frame) = decoder.next_frame()? else {
            return Err(ProtoError::corrupt(
                "frame",
                format!("truncated: {} bytes are not a whole frame", bytes.len()),
            ));
        };
        if decoder.buffered() != 0 {
            return Err(ProtoError::corrupt(
                "frame",
                format!("{} bytes after the frame", decoder.buffered()),
            ));
        }
        Ok(frame)
    }
}

/// The checksum of a frame: `crc32c(kind ++ request_id LE ++ body)`.
///
/// Public so that a test can compute it without rebuilding a frame, and so the one definition
/// is the one both sides use.
#[must_use]
pub fn frame_checksum(kind: FrameKind, request_id: u64, body: &[u8]) -> u32 {
    let mut state = crc32c::update(0, &[kind.as_u8()]);
    state = crc32c::update(state, &request_id.to_le_bytes());
    crc32c::update(state, body)
}

/// Reassembles frames from however TCP happens to deliver the bytes.
///
/// Feed it whatever arrives with [`FrameDecoder::push`] (or read straight into
/// [`FrameDecoder::buffer_mut`]) and take whole frames out with [`FrameDecoder::next_frame`]
/// until it returns `None`.
///
/// **A framing error poisons the decoder.** Once a length or a checksum is wrong, the position
/// of the next frame in the stream is unknown, and hunting for a plausible header is how a
/// reader resynchronises onto garbage. Every later call returns an error and the caller is
/// expected to close the connection.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: BytesMut,
    max_frame_size: usize,
    poisoned: bool,
}

impl FrameDecoder {
    /// A decoder that refuses any frame larger than `max_frame_size`.
    ///
    /// A limit below one header could never accept a frame, so it is raised to
    /// [`FRAME_HEADER_SIZE`]: a configuration mistake should make the smallest frames work,
    /// not make every frame fail with a corruption error.
    #[must_use]
    pub fn new(max_frame_size: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            max_frame_size: max_frame_size.max(FRAME_HEADER_SIZE),
            poisoned: false,
        }
    }

    /// Adds bytes that arrived from the peer.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// The buffer, so a socket can read straight into it without a copy.
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }

    /// How many bytes are held but not yet a whole frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// The largest frame this decoder accepts.
    #[must_use]
    pub fn max_frame_size(&self) -> usize {
        self.max_frame_size
    }

    /// The next whole frame, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtoError> {
        if self.poisoned {
            return Err(ProtoError::corrupt(
                "frame stream",
                "the position of the next frame was lost by an earlier framing error",
            ));
        }
        match self.decode_one() {
            Ok(frame) => Ok(frame),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    fn decode_one(&mut self) -> Result<Option<Frame>, ProtoError> {
        if self.buffer.len() < LEN_SIZE {
            return Ok(None);
        }
        let mut len_bytes = [0u8; LEN_SIZE];
        len_bytes.copy_from_slice(&self.buffer[..LEN_SIZE]);
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Both checks happen before the body is waited for, let alone allocated: a length
        // that cannot be right must cost an error now, not a 4 GiB buffer later.
        if len < MIN_LEN_FIELD {
            return Err(ProtoError::corrupt(
                "frame",
                format!("length field {len} is below the {MIN_LEN_FIELD}-byte minimum"),
            ));
        }
        let total = len + LEN_SIZE;
        if total > self.max_frame_size {
            return Err(ProtoError::corrupt(
                "frame",
                format!(
                    "frame of {total} bytes exceeds the {}-byte limit",
                    self.max_frame_size
                ),
            ));
        }
        if self.buffer.len() < total {
            return Ok(None);
        }

        let frame = self.buffer.split_to(total).freeze();
        // `crc` covers everything after itself: kind, request id and body, contiguous.
        let expected = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let actual = crc32c::checksum(&frame[LEN_SIZE + 4..]);
        if actual != expected {
            return Err(ProtoError::corrupt(
                "frame",
                format!(
                    "checksum mismatch: header says {expected:#010x}, bytes are {actual:#010x}"
                ),
            ));
        }

        // Only now is any of it interpreted.
        let Some(kind) = FrameKind::from_u8(frame[8]) else {
            return Err(ProtoError::corrupt(
                "frame",
                format!("unknown frame kind {}", frame[8]),
            ));
        };
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&frame[9..FRAME_HEADER_SIZE]);
        Ok(Some(Frame {
            kind,
            request_id: u64::from_le_bytes(id_bytes),
            body: frame.slice(FRAME_HEADER_SIZE..),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Frame, FrameDecoder, FrameKind, MAX_BODY_SIZE, MAX_FRAME_SIZE, MIN_LEN_FIELD,
        frame_checksum,
    };
    use bytes::{Bytes, BytesMut};

    fn round_trip(frame: &Frame) -> Frame {
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        Frame::decode(&bytes, MAX_FRAME_SIZE).unwrap()
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in FrameKind::ALL {
            let frame = Frame::new(kind, 0x0123_4567_89AB_CDEF, Bytes::from_static(b"payload"));
            assert_eq!(round_trip(&frame), frame);
        }
    }

    #[test]
    fn an_empty_body_round_trips() {
        let frame = Frame::empty(FrameKind::Ping, 0);
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        assert_eq!(bytes.len(), super::FRAME_HEADER_SIZE);
        assert_eq!(round_trip(&frame), frame);
    }

    /// Kind bytes are format, and zero must stay unusable. See the type's documentation.
    #[test]
    fn frame_kinds_are_distinct_and_nonzero() {
        let unique: std::collections::BTreeSet<u8> =
            FrameKind::ALL.into_iter().map(FrameKind::as_u8).collect();
        assert_eq!(unique.len(), FrameKind::ALL.len(), "two kinds share a byte");
        assert!(!unique.contains(&0), "zero must not be a valid kind");
        assert_eq!(FrameKind::from_u8(0), None);
        for kind in FrameKind::ALL {
            assert_eq!(FrameKind::from_u8(kind.as_u8()), Some(kind));
        }
    }

    /// The header layout is frozen; if it drifts, every peer on the old version loses the
    /// body.
    #[test]
    fn the_header_layout_is_frozen() {
        assert_eq!(super::FRAME_HEADER_SIZE, 17);
        assert_eq!(MIN_LEN_FIELD, 13);
        assert_eq!(MAX_FRAME_SIZE, 16 * 1024 * 1024);
        assert_eq!(MAX_BODY_SIZE, MAX_FRAME_SIZE - 17);

        let frame = Frame::new(FrameKind::Request, 1, Bytes::from_static(b"ab"));
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        assert_eq!(&bytes[..4], &15u32.to_le_bytes(), "len counts crc..body");
        assert_eq!(bytes[8], FrameKind::Request.as_u8());
        assert_eq!(&bytes[9..17], &1u64.to_le_bytes());
        assert_eq!(&bytes[17..], b"ab");
    }

    /// The one framing bug nothing above the framing could detect: a response delivered to
    /// the wrong caller, both frames otherwise intact. The checksum has to cover the id.
    #[test]
    fn the_checksum_covers_the_request_id_and_the_kind() {
        let frame = Frame::new(FrameKind::Response, 1, Bytes::from_static(b"value"));
        let mut bytes = frame.encode(MAX_FRAME_SIZE).unwrap().to_vec();
        bytes[9] ^= 0x01; // the low byte of the request id, and nothing else
        let error = Frame::decode(&bytes, MAX_FRAME_SIZE).unwrap_err();
        assert!(format!("{error}").contains("checksum"), "{error}");

        let mut bytes = frame.encode(MAX_FRAME_SIZE).unwrap().to_vec();
        bytes[8] = FrameKind::Error.as_u8();
        assert!(Frame::decode(&bytes, MAX_FRAME_SIZE).is_err());
    }

    /// Every byte of a frame is covered by the checksum except the length field, which is
    /// checked by other means. Flipping any one of them must be caught.
    #[test]
    fn a_flipped_bit_anywhere_is_caught() {
        let frame = Frame::new(
            FrameKind::Request,
            0xFFFF_0000_FFFF_0000,
            Bytes::from(vec![7; 40]),
        );
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        for offset in 4..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.to_vec();
                damaged[offset] ^= 1 << bit;
                assert!(
                    Frame::decode(&damaged, MAX_FRAME_SIZE).is_err(),
                    "a flip at byte {offset} bit {bit} decoded"
                );
            }
        }
    }

    #[test]
    fn an_oversized_frame_is_refused_on_the_way_out() {
        let frame = Frame::new(FrameKind::Request, 1, Bytes::from(vec![0; 64]));
        let error = frame.encode(32).unwrap_err();
        assert!(format!("{error}").contains("exceeds"), "{error}");
    }

    /// The read side has to refuse the length itself, before it waits for — or allocates —
    /// the body it claims.
    #[test]
    fn an_oversized_length_is_refused_before_the_body_arrives() {
        let mut decoder = FrameDecoder::new(1024);
        decoder.push(&100_000u32.to_le_bytes());
        let error = decoder.next_frame().unwrap_err();
        assert!(format!("{error}").contains("exceeds"), "{error}");
        assert_eq!(decoder.buffered(), 4, "nothing was consumed or allocated");
    }

    #[test]
    fn a_length_below_the_minimum_is_refused() {
        for len in 0..u32::try_from(MIN_LEN_FIELD).unwrap() {
            let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
            decoder.push(&len.to_le_bytes());
            assert!(decoder.next_frame().is_err(), "length {len} was accepted");
        }
    }

    #[test]
    fn an_unknown_kind_is_an_error_not_a_skipped_frame() {
        let frame = Frame::new(FrameKind::Request, 3, Bytes::from_static(b"x"));
        let mut bytes = frame.encode(MAX_FRAME_SIZE).unwrap().to_vec();
        bytes[8] = 9;
        // Repair the checksum, so that the kind is what fails rather than the CRC.
        let checksum = {
            let mut state = esker_base::crc32c::update(0, &[9]);
            state = esker_base::crc32c::update(state, &3u64.to_le_bytes());
            esker_base::crc32c::update(state, b"x")
        };
        bytes[4..8].copy_from_slice(&checksum.to_le_bytes());
        let error = Frame::decode(&bytes, MAX_FRAME_SIZE).unwrap_err();
        assert!(format!("{error}").contains("unknown frame kind"), "{error}");
    }

    /// Several frames in one buffer must come out one at a time, in order, with nothing left.
    #[test]
    fn a_batch_of_frames_decodes_in_order() {
        let frames: Vec<Frame> = (0u8..8)
            .map(|id| {
                Frame::new(
                    FrameKind::Request,
                    u64::from(id),
                    Bytes::from(vec![id; usize::from(id)]),
                )
            })
            .collect();
        let mut stream = BytesMut::new();
        for frame in &frames {
            frame.encode_into(&mut stream, MAX_FRAME_SIZE).unwrap();
        }

        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&stream);
        for expected in &frames {
            assert_eq!(decoder.next_frame().unwrap().as_ref(), Some(expected));
        }
        assert_eq!(decoder.next_frame().unwrap(), None);
        assert_eq!(decoder.buffered(), 0);
    }

    /// TCP splits wherever it likes. Feeding the same stream one byte at a time must produce
    /// exactly the same frames.
    #[test]
    fn a_frame_split_across_reads_decodes_identically() {
        let frame = Frame::new(FrameKind::Stream, 77, Bytes::from(vec![3; 300]));
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();

        let mut reader = FrameDecoder::new(MAX_FRAME_SIZE);
        for (index, byte) in bytes.iter().enumerate() {
            reader.push(&[*byte]);
            let taken = reader.next_frame().unwrap();
            if index + 1 == bytes.len() {
                assert_eq!(taken, Some(frame.clone()));
            } else {
                assert_eq!(taken, None, "a frame appeared after {} bytes", index + 1);
            }
        }
    }

    /// After a framing error the stream's position is unknown, so resynchronising is not
    /// something this decoder is allowed to try.
    #[test]
    fn a_framing_error_poisons_the_decoder() {
        let mut decoder = FrameDecoder::new(MAX_FRAME_SIZE);
        decoder.push(&0u32.to_le_bytes());
        assert!(decoder.next_frame().is_err());

        let good = Frame::empty(FrameKind::Ping, 1)
            .encode(MAX_FRAME_SIZE)
            .unwrap();
        decoder.push(&good);
        let error = decoder.next_frame().unwrap_err();
        assert!(format!("{error}").contains("lost"), "{error}");
    }

    #[test]
    fn the_checksum_helper_agrees_with_the_encoder() {
        let frame = Frame::new(FrameKind::Error, 5, Bytes::from_static(b"body"));
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        let expected = frame_checksum(frame.kind, frame.request_id, &frame.body);
        assert_eq!(&bytes[4..8], &expected.to_le_bytes());
    }

    #[test]
    fn only_the_kinds_that_end_an_exchange_are_terminal() {
        for kind in FrameKind::ALL {
            let expected = matches!(
                kind,
                FrameKind::Response | FrameKind::StreamEnd | FrameKind::Error
            );
            assert_eq!(kind.is_terminal(), expected, "{kind:?}");
        }
    }

    /// A single-frame decode is exact: fewer bytes than a frame, or more, is an error rather
    /// than a partial or a silently ignored tail.
    #[test]
    fn decoding_one_frame_refuses_a_short_or_long_buffer() {
        let frame = Frame::new(FrameKind::Request, 1, Bytes::from_static(b"abc"));
        let bytes = frame.encode(MAX_FRAME_SIZE).unwrap();
        assert!(Frame::decode(&bytes[..bytes.len() - 1], MAX_FRAME_SIZE).is_err());

        let mut longer = bytes.to_vec();
        longer.push(0);
        assert!(Frame::decode(&longer, MAX_FRAME_SIZE).is_err());
    }
}
