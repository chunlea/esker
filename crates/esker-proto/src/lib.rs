//! The wire protocol: hand-rolled length-prefixed frames over one TCP connection, carrying
//! multiplexed requests, responses and streams for `RawKV`, `TxnKV`, the placement driver and
//! Raft transport. There is no `tonic`, no `prost` and no `serde` — every message has an
//! `encode`/`decode` pair written by hand and pinned by golden tests (`docs/DESIGN.md` §9,
//! `docs/adr/0002-formats-are-hand-rolled.md`).
//!
//! # Invariants
//!
//! * **Every frame is checksummed** with CRC32C, like every byte on disk
//!   (`CLAUDE.md` invariant 2).
//! * **Unknown methods and unknown fields are errors, never ignored.** Compatibility is
//!   negotiated once, through [`WIRE_VERSION`] on connect, rather than guessed per message.
//! * **Every key-value request carries `{ region_id, epoch, peer }`** so the server can reject
//!   a stale epoch with a redirect hint (invariant 5).
//! * **Byte-opaque.** Keys and values cross the wire as byte strings; this crate never
//!   interprets them (invariant 7).
//!
//! Phase 0 contains only the frame constants; the protocol is phase 2
//! (`prompts/02-single-node-server.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Version of the framing and of every message encoding. Negotiated when a connection opens;
/// a mismatch is a hard error, not a downgrade.
pub const WIRE_VERSION: u16 = 1;

/// `len:u32 ++ crc32c:u32 ++ kind:u8 ++ request_id:u64` (`docs/DESIGN.md` §9).
pub const FRAME_HEADER_SIZE: usize = 4 + 4 + 1 + 8;

/// Largest frame that will be read or written. A larger declared length is rejected before
/// anything is allocated, so a corrupt or hostile length cannot exhaust memory.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Frame kinds (`docs/DESIGN.md` §9). One byte on the wire, so these values are format.
pub mod frame_kind {
    /// A request; the body starts with a `method:u16`.
    pub const REQUEST: u8 = 1;
    /// A response to the request with the same id.
    pub const RESPONSE: u8 = 2;
    /// One chunk of a stream, such as a snapshot transfer.
    pub const STREAM: u8 = 3;
    /// The final frame of a stream.
    pub const STREAM_END: u8 = 4;
    /// A typed error carrying redirect hints.
    pub const ERROR: u8 = 5;
    /// Liveness probe.
    pub const PING: u8 = 6;
    /// Reply to a [`PING`].
    pub const PONG: u8 = 7;

    /// Every kind this version defines.
    pub const ALL: [u8; 7] = [REQUEST, RESPONSE, STREAM, STREAM_END, ERROR, PING, PONG];
}

#[cfg(test)]
mod tests {
    use super::{FRAME_HEADER_SIZE, MAX_FRAME_SIZE, WIRE_VERSION, frame_kind};

    /// One byte on the wire per kind, and a zero would be indistinguishable from padding.
    #[test]
    fn frame_kinds_are_distinct_and_nonzero() {
        let unique: std::collections::BTreeSet<u8> = frame_kind::ALL.into_iter().collect();
        assert_eq!(
            unique.len(),
            frame_kind::ALL.len(),
            "two frame kinds share a tag"
        );
        assert!(!unique.contains(&0));
    }

    /// The header layout is fixed; if this drifts, every peer on the old version is unable to
    /// find the body.
    #[test]
    fn frame_header_layout_is_frozen() {
        assert_eq!(FRAME_HEADER_SIZE, 17);
        assert_eq!(WIRE_VERSION, 1);
    }

    /// A snapshot chunk plus its framing must fit in one frame, otherwise the store cannot
    /// send the chunk size it is configured for.
    #[test]
    fn max_frame_holds_a_snapshot_chunk() {
        assert!(MAX_FRAME_SIZE > esker_store_snapshot_chunk() + FRAME_HEADER_SIZE);
    }

    /// The value from `esker-store`, restated rather than imported: `esker-proto` sits below
    /// the store and must not depend on it.
    const fn esker_store_snapshot_chunk() -> usize {
        1024 * 1024
    }
}
